//! The session from inside the sandbox: KWin on a virtual output, the AT-SPI registry, window
//! geometry and placement through KWin scripts that call back over D-Bus, and capture through
//! ScreenShot2, and input through KWin's EIS.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gpui_pilot_protocol::{
    Answered, FrameReport, Modifier, MouseButton, Node, PickAnswer, PromptInfo, PromptKind,
    Recorder, Rect, Wheel,
};
use rustix::process::{Pid, Signal, kill_process};
use zbus::blocking::Connection;
use zbus::zvariant::{Fd, OwnedValue, Value};

use crate::SessionError;
use crate::clipboard;
use crate::eis::Eis;
use crate::isolation::check_desktop;
use crate::keymap::{Keymap, Run};
use crate::portal::{PortalWatch, dialog_text, file_uri_path, pending, response_after};
use crate::sandbox::{Output, WAYLAND_SOCKET};

const SESSION_NAME: &str = "org.gpuipilot.Session";
const REGISTRY: &str = "org.a11y.atspi.Registry";
const REGISTRYD: &str = "/usr/lib/at-spi2-registryd";
const KWIN: &str = "org.kde.KWin";
const START_TIMEOUT: Duration = Duration::from_secs(5);
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long `stop` lets each process exit on SIGTERM before killing it.
const STOP_GRACE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(100);
/// evdev button codes from linux/input-event-codes.h.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
/// The gap between a drag's events, one frame at 60 Hz.
const DRAG_STEP: Duration = Duration::from_millis(16);
/// QImage::Format_ARGB32_Premultiplied: BGRA bytes in memory.
const FORMAT_ARGB32_PREMULTIPLIED: u32 = 6;
/// QImage::Format_RGBX8888: R, G, B bytes and an unused fourth byte (KWin 6.8).
const FORMAT_RGBX8888: u32 = 16;
/// QImage::Format_RGBA8888: straight RGBA bytes.
const FORMAT_RGBA8888: u32 = 17;
/// QImage::Format_RGBA8888_Premultiplied: premultiplied RGBA bytes.
const FORMAT_RGBA8888_PREMULTIPLIED: u32 = 18;
/// The KDE portal backend, whose process owns every file dialog (spike §2).
const PORTAL_KDE: &str = "org.freedesktop.impl.portal.desktop.kde";
/// The id suffix of the dialog's location edit, the one name field in every mode (spike §3).
const LOCATION_EDIT: &str = "KFileWidget.QSplitter.QWidget.KUrlComboBox.KLineEdit";
const DIALOG_POLL: Duration = Duration::from_millis(50);
/// How long a dialog may take to respond to enter or to a cancelling escape.
const DIALOG_RESPONSE: Duration = Duration::from_secs(3);
const ESCAPE_RESPONSE: Duration = Duration::from_millis(1500);
/// The wait after each pasted run, so the app reads the clipboard before the next run sets it.
const PASTE_GAP: Duration = Duration::from_millis(200);

/// What `answer_prompt` did: how the prompt ended, what it asked, and the paths the app got.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub struct PromptAnswer {
    pub answered: Answered,
    pub prompt: PromptInfo,
    pub paths: Vec<PathBuf>,
}

/// An unanswered prompt whose dialog is on screen: the prompt's event index, the prompt, the
/// dialog's pid and the centre of its location edit in logical pixels.
struct ReadyDialog {
    index: usize,
    prompt: PromptInfo,
    pid: u32,
    edit: (f32, f32),
}

/// A window's outer frame, client area and wl_surface in KWin logical pixels, and the output
/// scale. The surface exceeds the client by a client-side decoration's shadow inset.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub struct WindowGeometry {
    pub frame: Rect,
    pub client: Rect,
    pub surface: Rect,
    pub scale: f32,
}

impl WindowGeometry {
    /// The client area relative to the surface origin, which AT-SPI window extents count from.
    pub fn content(&self) -> Rect {
        Rect::new(
            self.client.x - self.surface.x,
            self.client.y - self.surface.y,
            self.client.w,
            self.client.h,
        )
    }
}

/// A capture of the whole output in device pixels, as straight (not premultiplied) RGBA.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub struct Capture {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub rgba: Vec<u8>,
}

impl Capture {
    /// The RGBA value at device pixel (`x`, `y`); panics outside the image.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        assert!(
            x < self.width && y < self.height,
            "({x}, {y}) is outside the capture"
        );
        let at = ((y * self.width + x) * 4) as usize;
        self.rgba[at..at + 4].try_into().unwrap()
    }

    /// The part under `logical`, scaled to device pixels and clamped to the image.
    pub fn crop(&self, logical: &Rect) -> Capture {
        let device = |v: f32, limit: u32| ((v * self.scale).round().max(0.) as u32).min(limit);
        let x0 = device(logical.x, self.width);
        let y0 = device(logical.y, self.height);
        let x1 = (x0 + device(logical.w, self.width)).min(self.width);
        let y1 = (y0 + device(logical.h, self.height)).min(self.height);
        let mut rgba = Vec::with_capacity(((x1 - x0) * (y1 - y0) * 4) as usize);
        for y in y0..y1 {
            let row = ((y * self.width + x0) * 4) as usize;
            rgba.extend_from_slice(&self.rgba[row..row + ((x1 - x0) * 4) as usize]);
        }
        Capture {
            width: x1 - x0,
            height: y1 - y0,
            scale: self.scale,
            rgba,
        }
    }

    /// Writes the capture as a PNG file.
    pub fn save_png(&self, path: &Path) -> std::io::Result<()> {
        let image = image::RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
            .ok_or_else(|| std::io::Error::other("the pixel buffer does not match its size"))?;
        image
            .save_with_format(path, image::ImageFormat::Png)
            .map_err(std::io::Error::other)
    }
}

/// The callback a script run is waiting for: only JSON echoing `token` is kept, so a late reply
/// from an earlier, timed-out script is never read as this one's answer.
#[derive(Debug, Default)]
struct Pending {
    token: String,
    json: Option<String>,
}

impl Pending {
    fn expect(&mut self, token: &str) {
        self.token = token.to_owned();
        self.json = None;
    }

    fn offer(&mut self, json: String) {
        let echoed = serde_json::from_str::<serde_json::Value>(&json)
            .is_ok_and(|v| v["token"].as_str() == Some(self.token.as_str()));
        if echoed && self.json.is_none() {
            self.json = Some(json);
        }
    }
}

/// The pending callback, filled by `Sink` while a script run waits on it.
type Slot = Arc<(Mutex<Pending>, Condvar)>;

/// The recording the sink stamps into, with the token of the script feeding it.
type Recording = Arc<Mutex<Option<(String, Recorder)>>>;

/// Receives what KWin scripts send with `callDBus`: one-shot callbacks for the waiting script
/// run, and the frames script's damage and close signals for the current recording.
struct Sink {
    slot: Slot,
    recording: Recording,
}

impl Sink {
    fn offer(&self, json: String) {
        let (slot, ready) = &*self.slot;
        slot.lock().unwrap().offer(json);
        ready.notify_all();
    }
}

#[zbus::interface(name = "org.gpuipilot.Session")]
impl Sink {
    #[zbus(name = "Geometry")]
    fn geometry(&self, json: String) {
        self.offer(json);
    }

    #[zbus(name = "FramesReady")]
    fn frames_ready(&self, json: String) {
        self.offer(json);
    }

    #[zbus(name = "Frame")]
    fn frame(&self, token: String) {
        // Stamp on receipt, before any lock, so a busy agent thread cannot delay the stamp.
        let at = Instant::now();
        stamp_frame(&self.recording, &token, at);
    }

    #[zbus(name = "FramesEnded")]
    fn frames_ended(&self, token: String) {
        end_frames(&self.recording, &token);
    }
}

/// Stamps a frame at `at` when `token` names the current recording; drops it otherwise.
fn stamp_frame(recording: &Mutex<Option<(String, Recorder)>>, token: &str, at: Instant) {
    if let Some((current, recorder)) = recording.lock().unwrap().as_mut()
        && current == token
    {
        recorder.frame(at, None);
    }
}

/// Ends the current recording (truncated) when `token` names it.
fn end_frames(recording: &Mutex<Option<(String, Recorder)>>, token: &str) {
    if let Some((current, recorder)) = recording.lock().unwrap().as_mut()
        && current == token
    {
        recorder.end();
    }
}

/// The running private session. Dropping it stops the registry and KWin.
pub struct Session {
    output: Output,
    runtime: PathBuf,
    kwin: Child,
    registryd: Child,
    bus: Connection,
    a11y: Connection,
    slot: Slot,
    recording: Recording,
    /// The loaded frames script's name and file while a recording runs.
    frames: Option<(String, PathBuf)>,
    scripts: u32,
    eis: Eis,
    keymap: Keymap,
    watch: PortalWatch,
    /// Whether the compositor gave this session data control, probed once at start.
    clipboard: bool,
    /// The client rect of the last geometry read, and the pid it belongs to.
    client: Option<(u32, Rect)>,
}

impl Session {
    /// Starts KWin, the AT-SPI registry, the geometry callback and EIS input inside the
    /// sandbox. Refuses with `IsolationBreach` before starting anything if a desktop socket is
    /// reachable, and again before connecting input.
    pub fn start(output: Output) -> Result<Session, SessionError> {
        check_desktop()?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| launch("kwin", "XDG_RUNTIME_DIR is not set".into()))?;
        let mut kwin = Command::new("kwin_wayland")
            .args(["--virtual", "--no-lockscreen", "--no-global-shortcuts"])
            .args(["--socket", WAYLAND_SOCKET])
            .args(["--width", &output.width.to_string()])
            .args(["--height", &output.height.to_string()])
            .args(["--scale", &output.scale.to_string()])
            .env("KWIN_WAYLAND_NO_PERMISSION_CHECKS", "1")
            .env("KWIN_SCREENSHOT_NO_PERMISSION_CHECKS", "1")
            .spawn()
            .map_err(|e| launch("kwin", format!("spawning kwin_wayland: {e}")))?;
        // From here on every failure must reap KWin, so the rest runs in a closure.
        let started = (|| {
            let socket = runtime.join(WAYLAND_SOCKET);
            wait_for(START_TIMEOUT, || Ok(socket.exists()))?
                .then_some(())
                .ok_or_else(|| {
                    launch("kwin", format!("no socket {} within 5 s", socket.display()))
                })?;
            let bus = Connection::session().map_err(|e| launch("kwin", e.to_string()))?;
            // The socket appears before KWin owns its bus name, which EIS and scripts need.
            let named = wait_for(START_TIMEOUT, || {
                bus.call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus"),
                    "NameHasOwner",
                    &(KWIN,),
                )
                .and_then(|reply| reply.body().deserialize::<bool>())
                .map_err(|e| launch("kwin", e.to_string()))
            })?;
            if !named {
                return Err(launch("kwin", format!("{KWIN} did not appear within 5 s")));
            }
            bus.call_method(
                Some("org.a11y.Bus"),
                "/org/a11y/bus",
                Some("org.freedesktop.DBus.Properties"),
                "Set",
                &("org.a11y.Status", "IsEnabled", Value::from(true)),
            )
            .map_err(|e| launch("atspi", format!("enabling accessibility: {e}")))?;
            Ok::<_, SessionError>(bus)
        })();
        let bus = match started {
            Ok(bus) => bus,
            Err(error) => {
                reap(&mut kwin);
                return Err(error);
            }
        };
        let mut registryd = match Command::new(REGISTRYD).spawn() {
            Ok(child) => child,
            Err(e) => {
                reap(&mut kwin);
                return Err(launch("atspi", format!("spawning {REGISTRYD}: {e}")));
            }
        };
        let slot: Slot = Arc::default();
        let recording: Recording = Arc::default();
        let rest = (|| {
            let a11y = connect_a11y(&bus)?;
            bus.request_name(SESSION_NAME)?;
            bus.object_server().at(
                "/",
                Sink {
                    slot: slot.clone(),
                    recording: recording.clone(),
                },
            )?;
            check_desktop()?;
            let (eis, keymap) = Eis::connect(&bus)?;
            // A breach keeps its own code; any other watch failure is a launch stage.
            let watch = PortalWatch::start().map_err(|e| match e {
                SessionError::IsolationBreach { .. } => e,
                other => launch("portal", other.to_string()),
            })?;
            Ok::<_, SessionError>((a11y, eis, keymap, watch))
        })();
        let (a11y, eis, keymap, watch) = match rest {
            Ok(rest) => rest,
            Err(error) => {
                reap(&mut registryd);
                reap(&mut kwin);
                return Err(error);
            }
        };
        // Any failure here (no data control, or a refused display) means the session simply
        // has no clipboard; paste typing and `clipboard` then report it.
        let clipboard = clipboard::get_text().is_ok();
        let session = Session {
            output,
            runtime,
            kwin,
            registryd,
            bus,
            a11y,
            slot,
            recording,
            frames: None,
            scripts: 0,
            eis,
            keymap,
            watch,
            clipboard,
            client: None,
        };
        Ok(session)
    }

    /// The virtual output the session runs on.
    pub fn output(&self) -> Output {
        self.output
    }

    /// The keymap KWin's EIS sent, for checking text and keys before anything is sent.
    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// `IsolationBreach` if any desktop socket is reachable from here.
    pub fn check_isolation(&self) -> Result<(), SessionError> {
        check_desktop()
    }

    /// The current mode's refresh as the private compositor advertises it through `wl_output`,
    /// in mHz. `LaunchFailed` stage `output` when the socket, the global or the mode is missing.
    pub fn refresh_mhz(&self) -> Result<u32, SessionError> {
        crate::refresh::read_refresh_mhz(&self.runtime.join(WAYLAND_SOCKET))
    }

    /// Polls `window` until the process `pid` has a window or `timeout` passes, returning the
    /// last error then.
    pub fn wait_window(
        &mut self,
        pid: u32,
        timeout: Duration,
    ) -> Result<WindowGeometry, SessionError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.window(pid) {
                Ok(geometry) => return Ok(geometry),
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => std::thread::sleep(POLL),
            }
        }
    }

    /// The geometry of the normal window owned by `pid`: `LaunchFailed` (stage `window`) when
    /// it has none, `Timeout` when the geometry script does not call back within 2 s.
    pub fn window(&mut self, pid: u32) -> Result<WindowGeometry, SessionError> {
        let json = self.run_script("geom", |token| Ok(geometry_script(token)))?;
        let geometry = parse_geometry(&json, pid)?;
        self.client = Some((pid, geometry.client.clone()));
        Ok(geometry)
    }

    /// Runs the geometry script and answers `newest_window_above` over it.
    pub fn newer_window(
        &mut self,
        than: u32,
        members: &[u32],
    ) -> Result<Option<u32>, SessionError> {
        let json = self.run_script("geom", |token| Ok(geometry_script(token)))?;
        newest_window_above(&json, than, members)
    }

    /// Moves and resizes the window of `pid` so its frame is `frame` (KWin logical pixels),
    /// then returns its geometry once KWin reports that frame (or after 2 s, as it stands).
    pub fn place(&mut self, pid: u32, frame: Rect) -> Result<WindowGeometry, SessionError> {
        self.assign(pid, frame, false)
    }

    /// As `place`, after taking the window out of maximised and fullscreen states, which would
    /// otherwise keep their own size. The app's minimum size can still win; the geometry
    /// returned is what KWin reports.
    pub fn resize(&mut self, pid: u32, frame: Rect) -> Result<WindowGeometry, SessionError> {
        self.assign(pid, frame, true)
    }

    fn assign(
        &mut self,
        pid: u32,
        frame: Rect,
        restore: bool,
    ) -> Result<WindowGeometry, SessionError> {
        self.run_script("place", |token| {
            placement_script(token, pid, &frame, restore)
        })?;
        // The client acknowledges the resize asynchronously, so the frame lags the assignment.
        let deadline = Instant::now() + SCRIPT_TIMEOUT;
        loop {
            let geometry = self.window(pid)?;
            if geometry.frame == frame || Instant::now() >= deadline {
                return Ok(geometry);
            }
            std::thread::sleep(POLL);
        }
    }

    /// Asks KWin to close `pid`'s normal windows, as their close button would, then waits up
    /// to `wait` for them to go. Ok(true) once `pid` has no window, Ok(false) if one remains.
    pub fn close_window(&mut self, pid: u32, wait: Duration) -> Result<bool, SessionError> {
        self.run_script("close", |token| Ok(close_script(token, pid)))?;
        // The app decides on the close asynchronously, so "gone" is the geometry losing `pid`.
        let deadline = Instant::now() + wait;
        loop {
            match self.window(pid) {
                Err(SessionError::LaunchFailed {
                    stage: "window", ..
                }) => return Ok(true),
                Err(error) => return Err(error),
                Ok(_) if Instant::now() >= deadline => return Ok(false),
                Ok(_) => std::thread::sleep(POLL),
            }
        }
    }

    /// The app's accessible nodes below its application object, sorted by top, left, then
    /// path. `LaunchFailed` (stage `atspi`) when no application on the bus belongs to `pid`.
    pub fn tree(&mut self, pid: u32) -> Result<Vec<Node>, SessionError> {
        let a11y = self.a11y.clone();
        let scale = self.output.scale;
        crate::tree::walk(&a11y, pid, scale, || self.window(pid).map(|g| g.content()))
    }

    /// Captures the whole output at native resolution.
    pub fn capture(&mut self) -> Result<Capture, SessionError> {
        let (mut reader, writer) = std::io::pipe()?;
        let options: HashMap<&str, Value> = HashMap::from([("native-resolution", true.into())]);
        let reply = self.bus.call_method(
            Some(KWIN),
            "/org/kde/KWin/ScreenShot2",
            Some("org.kde.KWin.ScreenShot2"),
            "CaptureWorkspace",
            &(options, Fd::from(writer.as_fd())),
        )?;
        // KWin writes the pixels after replying; our write end must close for EOF to arrive.
        drop(writer);
        let meta: HashMap<String, OwnedValue> = reply.body().deserialize()?;
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;
        let int = |key: &str| {
            meta.get(key)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| SessionError::Internal(format!("ScreenShot2 reply lacks {key}")))
        };
        let (width, height, stride, format) = (
            int("width")?,
            int("height")?,
            int("stride")?,
            int("format")?,
        );
        let scale = meta
            .get("scale")
            .and_then(|v| f64::try_from(v).ok())
            .ok_or_else(|| SessionError::Internal("ScreenShot2 reply lacks scale".into()))?;
        check_capture_geometry(format, width, height, stride, data.len())?;
        let rgba = to_rgba(format, &data, width, height, stride)?;
        Ok(Capture {
            width,
            height,
            scale: scale as f32,
            rgba,
        })
    }

    /// Clicks `button` `count` times at `at`, window-content logical pixels of `pid`'s window,
    /// with `modifiers` held as `scroll` holds them; `InvalidKeystroke` before any input when
    /// the session keymap lacks a modifier's key.
    pub fn click(
        &mut self,
        pid: u32,
        at: (f32, f32),
        button: MouseButton,
        count: u32,
        modifiers: &[Modifier],
    ) -> Result<(), SessionError> {
        let code = evdev_button(button)?;
        self.point_holding(pid, at, modifiers, |session| {
            (0..count).try_for_each(|_| session.eis.button(code))
        })
    }

    /// Presses `button` at `from`, moves through `steps` points to `to` 16 ms apart, and
    /// releases there; points are window-content logical px of `pid`'s window. `modifiers`
    /// are held from before the press until after the release, as `scroll` holds them.
    pub fn drag(
        &mut self,
        pid: u32,
        from: (f32, f32),
        to: (f32, f32),
        steps: u32,
        button: MouseButton,
        modifiers: &[Modifier],
    ) -> Result<(), SessionError> {
        let code = evdev_button(button)?;
        // Each event waits a frame's length so KWin delivers the press, every held move and the
        // release as separate Wayland events.
        self.point_holding(pid, from, modifiers, |session| {
            std::thread::sleep(DRAG_STEP);
            held(
                session,
                |session| session.eis.press(code),
                |session| {
                    drag_points(from, to, steps).into_iter().try_for_each(|at| {
                        std::thread::sleep(DRAG_STEP);
                        session.point(pid, at)
                    })
                },
                |session| {
                    std::thread::sleep(DRAG_STEP);
                    session.eis.release(code)
                },
            )
        })
    }

    /// Moves the pointer to `at` in `pid`'s window content.
    pub fn hover(&mut self, pid: u32, at: (f32, f32)) -> Result<(), SessionError> {
        self.point(pid, at)
    }

    /// Moves to `at` in `pid`'s window content, presses `modifiers`, scrolls by (`dx`, `dy`)
    /// and releases them in reverse; `InvalidKeystroke` before any input when the session
    /// keymap lacks a modifier's key.
    pub fn scroll(
        &mut self,
        pid: u32,
        at: (f32, f32),
        dx: f32,
        dy: f32,
        modifiers: &[Modifier],
    ) -> Result<(), SessionError> {
        self.point_holding(pid, at, modifiers, |session| session.eis.scroll(dx, dy))
    }

    /// Moves to `at` in `pid`'s window, presses `modifiers` as `scroll` does, sends each notch of
    /// `wheel` (120 units, horizontal first) on absolute deadlines `notch_ms` apart, stamping
    /// each into the recording if one runs, then releases the modifiers. Trusts the counts.
    pub fn notches(
        &mut self,
        pid: u32,
        at: (f32, f32),
        wheel: &Wheel,
        modifiers: &[Modifier],
    ) -> Result<(), SessionError> {
        let units = notch_units(wheel);
        let period = Duration::from_millis(wheel.notch_ms);
        self.point_holding(pid, at, modifiers, |session| {
            pace(Instant::now(), period, units.len(), |i, scheduled| {
                let (x, y) = units[i];
                if let Some((_, recorder)) = session.recording.lock().unwrap().as_mut() {
                    recorder.notch(scheduled, Instant::now());
                }
                session.eis.notch(x, y)
            })
        })
    }

    /// The shared prologue of wheel, click and drag: resolves `modifiers` to key codes
    /// (`InvalidKeystroke` before any input when one is missing), moves to `at`, then runs
    /// `body` with them held.
    fn point_holding(
        &mut self,
        pid: u32,
        at: (f32, f32),
        modifiers: &[Modifier],
        body: impl FnOnce(&mut Session) -> Result<(), SessionError>,
    ) -> Result<(), SessionError> {
        let mut codes = Vec::new();
        for &modifier in modifiers {
            let code = self.keymap.modifier(modifier).ok_or_else(|| {
                SessionError::InvalidKeystroke(format!("{modifier:?}").to_lowercase())
            })?;
            if !codes.contains(&code) {
                codes.push(code);
            }
        }
        self.point(pid, at)?;
        if codes.is_empty() {
            return body(self);
        }
        // A frame's length between steps lets KWin deliver the modifiers before the axis
        // event; once pressed they are always released, so no modifier stays held.
        held(
            self,
            |session| session.eis.keys_down(&codes),
            |session| {
                std::thread::sleep(DRAG_STEP);
                body(session)
            },
            |session| {
                std::thread::sleep(DRAG_STEP);
                session.eis.keys_up(&codes)
            },
        )
    }

    /// Types `text` into the focused window; `UnsupportedCharacter` before any key is sent
    /// when the session keymap cannot type one of its characters.
    pub fn type_text(&mut self, text: &str) -> Result<(), SessionError> {
        let chords = self.keymap.text(text)?;
        chords.iter().try_for_each(|chord| self.eis.chord(chord))
    }

    /// Sends space-separated GPUI keystrokes such as `backspace shift-a`, all checked first.
    pub fn keys(&mut self, keys: &str) -> Result<(), SessionError> {
        let chords = self.keymap.sequence(keys)?;
        chords.iter().try_for_each(|chord| self.eis.chord(chord))
    }

    /// Sets the clipboard when `set` is given, then reads it. `Unsupported` without data control.
    pub fn clipboard(&mut self, set: Option<&str>) -> Result<Option<String>, SessionError> {
        self.require_clipboard()?;
        if let Some(text) = set {
            clipboard::set_text(text)?;
        }
        clipboard::get_text()
    }

    /// `UnsupportedCharacter` naming the first character that would need a paste, when the
    /// session has no data control; `Ok` otherwise.
    pub fn check_text(&self, text: &str) -> Result<(), SessionError> {
        if self.clipboard {
            return Ok(());
        }
        self.keymap.text(text).map(|_| ())
    }

    /// Types `text`: keymap runs as keys, other runs pasted with `ctrl-v`, each paste followed
    /// by `PASTE_GAP` (200 ms); each paste waits until the compositor offers the new selection.
    /// Calls `check_text` first.
    pub fn type_or_paste(&mut self, text: &str) -> Result<(), SessionError> {
        self.check_text(text)?;
        for run in self.keymap.runs(text) {
            match run {
                Run::Keys(chords) => chords.iter().try_for_each(|chord| self.eis.chord(chord))?,
                Run::Paste(pasted) => {
                    clipboard::set_text(&pasted)?;
                    self.keys("ctrl-v")?;
                    std::thread::sleep(PASTE_GAP);
                }
            }
        }
        Ok(())
    }

    fn require_clipboard(&self) -> Result<(), SessionError> {
        if self.clipboard {
            return Ok(());
        }
        Err(SessionError::Unsupported(
            "the session compositor offers no data control, so it has no clipboard".into(),
        ))
    }

    /// Waits up to `timeout` for an unanswered FileChooser prompt whose KDE dialog shows its
    /// location edit, then answers it: `Cancel` presses escape; paths click the edit, select
    /// all, type or paste `dialog_text(paths)`, press enter, and require the portal's code 0
    /// response with exactly those paths. `Timeout` when no dialog is ready in time;
    /// `InvalidArgument` for several paths on a save or a save to an existing path (dialog left
    /// open), or a dialog that did not respond within 3 s of enter (dialog cancelled);
    /// `Internal` when it answered other paths.
    pub fn answer_prompt(
        &mut self,
        answer: &PickAnswer,
        timeout: Duration,
    ) -> Result<PromptAnswer, SessionError> {
        let ready = self.ready_dialog(timeout)?;
        let paths = match answer {
            PickAnswer::Paths(paths) => paths,
            _ => {
                self.keys("escape")?;
                return match self.response(ready.index, DIALOG_RESPONSE) {
                    Some(_) => Ok(PromptAnswer {
                        answered: Answered::Cancelled,
                        prompt: ready.prompt,
                        paths: Vec::new(),
                    }),
                    None => Err(SessionError::Timeout(
                        "the file dialog did not respond to escape within 3 s".into(),
                    )),
                };
            }
        };
        // Refusals that need no input leave the dialog open for another answer.
        let invalid = SessionError::InvalidArgument;
        if ready.prompt.kind == PromptKind::Save {
            if paths.len() > 1 {
                return Err(invalid(format!(
                    "a save dialog takes one path, not {}",
                    paths.len()
                )));
            }
            if let Some(existing) = paths.iter().find(|p| p.exists()) {
                return Err(invalid(format!(
                    "{} exists, and overwriting a file is unsupported",
                    existing.display()
                )));
            }
        }
        let text = dialog_text(paths)?;
        self.check_text(&text)?;
        // A folder dialog opens with focus on its accept button, where enter would return the
        // current folder, so the edit is clicked first (spike §4). Every dialog is a new window
        // of the same portal pid, so its geometry is read fresh rather than taken from the cache.
        self.window(ready.pid)?;
        self.click(ready.pid, ready.edit, MouseButton::Left, 1, &[])?;
        self.keys("ctrl-a")?;
        self.type_or_paste(&text)?;
        self.keys("enter")?;
        let Some((code, uris)) = self.response(ready.index, DIALOG_RESPONSE) else {
            // Typically a missing file: the dialog waits silently, so cancel it rather than
            // leave a prompt the next pick would type into.
            for _ in 0..2 {
                self.keys("escape")?;
                if self.response(ready.index, ESCAPE_RESPONSE).is_some() {
                    break;
                }
            }
            return Err(invalid(format!(
                "the file dialog did not accept {text} (an open needs an existing file); it was cancelled"
            )));
        };
        if code != 0 {
            return Err(invalid(format!(
                "the file dialog answered {text} with code {code}, not 0"
            )));
        }
        let got: Vec<PathBuf> = uris.iter().filter_map(|u| file_uri_path(u)).collect();
        let same = got.len() == uris.len()
            && got.len() == paths.len()
            && got
                .iter()
                .zip(paths)
                .all(|(a, b)| a.as_path() == b.as_path());
        if !same {
            return Err(SessionError::Internal(format!(
                "the file dialog answered {uris:?}, not {text}"
            )));
        }
        let answered = match ready.prompt.kind {
            PromptKind::Save => Answered::Save,
            _ => Answered::Open,
        };
        Ok(PromptAnswer {
            answered,
            prompt: ready.prompt,
            paths: got,
        })
    }

    /// Polls until the last prompt has no response and the portal's dialog shows exactly one
    /// visible location edit; any bus, tree or geometry error meanwhile means "not ready".
    fn ready_dialog(&mut self, timeout: Duration) -> Result<ReadyDialog, SessionError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(ready) = self.dialog_now() {
                return Ok(ready);
            }
            if Instant::now() >= deadline {
                return Err(SessionError::Timeout(format!(
                    "no file dialog was waiting for an answer within {} ms",
                    timeout.as_millis()
                )));
            }
            std::thread::sleep(DIALOG_POLL);
        }
    }

    /// One look for a ready dialog, as `ready_dialog` describes.
    fn dialog_now(&mut self) -> Option<ReadyDialog> {
        let (index, prompt) = pending(&self.watch.events())?;
        let pid: u32 = self
            .bus
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetConnectionUnixProcessID",
                &(PORTAL_KDE,),
            )
            .ok()?
            .body()
            .deserialize()
            .ok()?;
        // Qt reports logical extents, so the dialog's tree is read at scale 1 (spike §3).
        let a11y = self.a11y.clone();
        let nodes =
            crate::tree::walk(&a11y, pid, 1.0, || self.window(pid).map(|g| g.content())).ok()?;
        let mut edits = nodes.iter().filter(|n| {
            n.id.as_deref()
                .is_some_and(|id| id.ends_with(LOCATION_EDIT))
                && n.bounds.w > 0.
        });
        let edit = edits.next()?;
        if edits.next().is_some() {
            return None;
        }
        let b = &edit.bounds;
        Some(ReadyDialog {
            index,
            prompt,
            pid,
            edit: (b.x + b.w / 2., b.y + b.h / 2.),
        })
    }

    /// The first response to the prompt at `index`, polled every 50 ms for up to `wait`.
    fn response(&self, index: usize, wait: Duration) -> Option<(u32, Vec<String>)> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(response) = response_after(&self.watch.events(), index) {
                return Some(response);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(DIALOG_POLL);
        }
    }

    /// Starts recording `pid`'s one normal window: creates the recorder, loads the long-lived
    /// frames script and waits up to 2 s for its `FramesReady`. Already recording is
    /// `InvalidArgument` and leaves the running recording untouched; then `LaunchFailed {
    /// stage: "window" }` with no window, `Unsupported` with several, `Timeout`/`Internal` from
    /// the script. Each of those unloads this call's script and clears its recorder.
    pub fn frames_start(
        &mut self,
        pid: u32,
        refresh_mhz: u32,
        idle_ms: u64,
    ) -> Result<(), SessionError> {
        if self.frames.is_some() {
            return Err(SessionError::InvalidArgument(
                "frames are already recording; stop them first".into(),
            ));
        }
        let name = self.script_name("frames");
        let source = frames_script(&name, pid);
        // The recorder exists before the script loads, so damage ahead of `FramesReady` counts.
        let recorder = Recorder::new(Instant::now(), refresh_mhz, idle_ms);
        *self.recording.lock().unwrap() = Some((name.clone(), recorder));
        let loaded = self.load_script(&name, source).and_then(|(path, json)| {
            let windows = serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .and_then(|v| v["windows"].as_u64());
            let refused = match windows {
                Some(1) => return Ok(path),
                Some(0) => launch("window", format!("no window of pid {pid} to record")),
                Some(_) => {
                    SessionError::Unsupported("frames records an app with one window".into())
                }
                None => SessionError::Internal(format!("bad FramesReady callback: {json}")),
            };
            self.unload_script(&name, &path);
            Err(refused)
        });
        match loaded {
            Ok(path) => {
                self.frames = Some((name, path));
                Ok(())
            }
            Err(error) => {
                *self.recording.lock().unwrap() = None;
                Err(error)
            }
        }
    }

    /// Unloads the frames script and reports what was stamped (`work` and `build` absent);
    /// `InvalidArgument` when not recording.
    pub fn frames_stop(&mut self) -> Result<FrameReport, SessionError> {
        let (name, path) = self
            .frames
            .take()
            .ok_or_else(|| SessionError::InvalidArgument("frames are not recording".into()))?;
        // Taking the recorder first makes any damage still in flight a dropped stale stamp.
        let stopped = Instant::now();
        let recording = self.recording.lock().unwrap().take();
        self.unload_script(&name, &path);
        let (_, recorder) = recording.ok_or_else(|| {
            SessionError::Internal("the frames recorder vanished while recording".into())
        })?;
        Ok(recorder.finish(stopped, None))
    }

    /// True once the recorded window's `FramesEnded` has arrived; false when not recording.
    pub fn frames_ended(&self) -> bool {
        // A full recorder also reads as ended, which is harmless for a poll.
        self.recording
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(_, recorder)| recorder.finish(Instant::now(), None).truncated)
    }

    /// Moves the pointer to a content point: EIS takes KWin logical pixels, the client origin
    /// plus the point. The origin comes from the last geometry read for `pid`, which holds
    /// while only the agent moves the window; with none cached it is read fresh.
    fn point(&mut self, pid: u32, (x, y): (f32, f32)) -> Result<(), SessionError> {
        let client = match cached_client(&self.client, pid) {
            Some(client) => client,
            None => self.window(pid)?.client,
        };
        self.eis.motion(client.x + x, client.y + y)
    }

    /// Loads the script `source` builds for its unique name, runs it, waits up to 2 s for the
    /// callback echoing that name, and unloads it. Returns the callback's JSON, or `Timeout`
    /// naming the script when it never calls back.
    fn run_script(
        &mut self,
        kind: &str,
        source: impl FnOnce(&str) -> Result<String, SessionError>,
    ) -> Result<String, SessionError> {
        let name = self.script_name(kind);
        let source = source(&name)?;
        let (path, json) = self.load_script(&name, source)?;
        self.unload_script(&name, &path);
        Ok(json)
    }

    /// The next unique script name for `kind`, which is also the token its callback echoes.
    fn script_name(&mut self, kind: &str) -> String {
        self.scripts += 1;
        format!("gp-{kind}-{}", self.scripts)
    }

    /// Loads and starts `source` as script `name` and waits up to 2 s for its callback. On
    /// success the script stays loaded and its file path and callback JSON are returned; on any
    /// failure it is unloaded first.
    fn load_script(
        &mut self,
        name: &str,
        source: String,
    ) -> Result<(PathBuf, String), SessionError> {
        self.slot.0.lock().unwrap().expect(name);
        let path = self.runtime.join(format!("{name}.js"));
        std::fs::write(&path, source)?;
        let result = self
            .bus
            .call_method(
                Some(KWIN),
                "/Scripting",
                Some("org.kde.kwin.Scripting"),
                "loadScript",
                &(path.to_string_lossy().as_ref(), name),
            )
            .and_then(|reply| reply.body().deserialize::<i32>())
            .map_err(SessionError::from)
            .and_then(|id| match id {
                id if id < 0 => Err(SessionError::Internal(format!(
                    "KWin refused script {name} (loadScript returned {id})"
                ))),
                _ => self
                    .bus
                    .call_method(
                        Some(KWIN),
                        "/Scripting",
                        Some("org.kde.kwin.Scripting"),
                        "start",
                        &(),
                    )
                    .map(|_| ())
                    .map_err(SessionError::from),
            });
        // KWin runs a started script later, on its own loop: unloading before the callback
        // arrives can drop the script unrun, so every script ends by calling back.
        let result = result.and_then(|()| {
            let (slot, ready) = &*self.slot;
            let guard = slot.lock().unwrap();
            let (mut guard, _) = ready
                .wait_timeout_while(guard, SCRIPT_TIMEOUT, |pending| pending.json.is_none())
                .unwrap();
            guard.json.take().ok_or_else(|| {
                SessionError::Timeout(format!("KWin script {name} did not call back within 2 s"))
            })
        });
        match result {
            Ok(json) => Ok((path, json)),
            Err(error) => {
                self.unload_script(name, &path);
                Err(error)
            }
        }
    }

    /// Unloads script `name` and removes its file; both are best effort.
    fn unload_script(&self, name: &str, path: &Path) {
        let _ = self.bus.call_method(
            Some(KWIN),
            "/Scripting",
            Some("org.kde.kwin.Scripting"),
            "unloadScript",
            &(name,),
        );
        let _ = std::fs::remove_file(path);
    }
}

impl Session {
    /// Stops the registry, then KWin, each through `terminate` with 2 s of grace. `Drop` stays
    /// as the fallback and finds both already reaped after a `stop`.
    pub fn stop(mut self) -> Result<(), SessionError> {
        terminate(&mut self.registryd, STOP_GRACE)?;
        terminate(&mut self.kwin, STOP_GRACE)?;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        reap(&mut self.registryd);
        reap(&mut self.kwin);
    }
}

/// Calls back with every normal window, since `getWindowInfo` has no client geometry, and
/// echoes `token` so the reply can be matched to this run.
fn geometry_script(token: &str) -> String {
    let token = serde_json::to_string(token).expect("a string serializes");
    format!(
        r#"const out = [];
for (const w of workspace.windowList()) if (w.normalWindow) out.push({{pid: w.pid,
  frame: [w.frameGeometry.x, w.frameGeometry.y, w.frameGeometry.width, w.frameGeometry.height],
  client: [w.clientGeometry.x, w.clientGeometry.y, w.clientGeometry.width, w.clientGeometry.height],
  surface: [w.bufferGeometry.x, w.bufferGeometry.y, w.bufferGeometry.width, w.bufferGeometry.height]}});
callDBus("org.gpuipilot.Session", "/", "org.gpuipilot.Session", "Geometry",
  JSON.stringify({{token: {token}, scale: workspace.screens[0].devicePixelRatio, windows: out}}));
"#
    )
}

/// Assigns `frame` to `pid`'s normal window, with `restore` first un-maximising it and leaving
/// fullscreen, then reports geometry so `run_script` knows the assignment ran. `Internal` for a
/// non-finite frame, which would not be valid script.
fn placement_script(
    token: &str,
    pid: u32,
    frame: &Rect,
    restore: bool,
) -> Result<String, SessionError> {
    if ![frame.x, frame.y, frame.w, frame.h]
        .iter()
        .all(|v| v.is_finite())
    {
        return Err(SessionError::Internal(format!(
            "cannot place a window at a frame that is not finite: {frame:?}"
        )));
    }
    let restore = if restore {
        "w.setMaximize(false, false);\n  w.fullScreen = false;\n  "
    } else {
        ""
    };
    Ok(format!(
        "for (const w of workspace.windowList()) if (w.normalWindow && w.pid == {pid}) {{\n  \
         {restore}w.frameGeometry = {{x: {}, y: {}, width: {}, height: {}}};\n}}\n{}",
        frame.x,
        frame.y,
        frame.w,
        frame.h,
        geometry_script(token)
    ))
}

/// Asks `pid`'s normal windows to close, which KWin sends as `xdg_toplevel.close`, then
/// reports geometry so `run_script` knows the request ran.
fn close_script(token: &str, pid: u32) -> String {
    format!(
        "for (const w of workspace.windowList()) if (w.normalWindow && w.pid == {pid})\n  \
         w.closeWindow();\n{}",
        geometry_script(token)
    )
}

/// Hooks `pid`'s one normal window: each damage calls `Frame` and its close calls
/// `FramesEnded`, both echoing `token`; then reports how many windows matched in `FramesReady`.
/// With no window or several nothing is hooked, so the caller can refuse and unload.
fn frames_script(token: &str, pid: u32) -> String {
    let token = serde_json::to_string(token).expect("a string serializes");
    format!(
        r#"const token = {token};
const send = (member, arg) => callDBus("org.gpuipilot.Session", "/", "org.gpuipilot.Session", member, arg);
const found = workspace.windowList().filter(w => w.normalWindow && w.pid == {pid});
if (found.length == 1) {{
  found[0].damaged.connect(() => send("Frame", token));
  found[0].closed.connect(() => send("FramesEnded", token));
}}
send("FramesReady", JSON.stringify({{token: token, windows: found.length}}));
"#
    )
}

/// Runs `body` between `press` and `release`. Once the press succeeds the release is always
/// attempted, so a failed move never leaves the button held; the first error is returned.
fn held<S, E>(
    state: &mut S,
    press: impl FnOnce(&mut S) -> Result<(), E>,
    body: impl FnOnce(&mut S) -> Result<(), E>,
    release: impl FnOnce(&mut S) -> Result<(), E>,
) -> Result<(), E> {
    press(state)?;
    let moved = body(state);
    let released = release(state);
    moved.and(released)
}

/// The (x, y) units of each notch: horizontal ones first, then vertical, ±120 each.
fn notch_units(wheel: &Wheel) -> Vec<(i32, i32)> {
    let x = std::iter::repeat_n(
        (wheel.notches_x.signum() * 120, 0),
        wheel.notches_x.unsigned_abs() as usize,
    );
    let y = std::iter::repeat_n(
        (0, wheel.notches_y.signum() * 120),
        wheel.notches_y.unsigned_abs() as usize,
    );
    x.chain(y).collect()
}

/// Calls `send(i, scheduled)` for `i` in `0..count` at `first + i * period`; an overdue notch
/// goes at once and never delays the next. Stops at the first error.
fn pace<E>(
    first: Instant,
    period: Duration,
    count: usize,
    mut send: impl FnMut(usize, Instant) -> Result<(), E>,
) -> Result<(), E> {
    // Each deadline counts from `first`, never from the previous send, so a stall is absorbed
    // by the notches after it instead of shifting all of them.
    let mut scheduled = first;
    for i in 0..count {
        std::thread::sleep(scheduled.saturating_duration_since(Instant::now()));
        send(i, scheduled)?;
        scheduled += period;
    }
    Ok(())
}

/// The points a drag moves through after its press: `steps` (at least one) even steps from
/// `from`, the last exactly `to`, as the host's `drag_path`.
/// The evdev code EIS sends for `button`.
fn evdev_button(button: MouseButton) -> Result<u32, SessionError> {
    match button {
        MouseButton::Left => Ok(BTN_LEFT),
        MouseButton::Right => Ok(BTN_RIGHT),
        MouseButton::Middle => Ok(BTN_MIDDLE),
        other => Err(SessionError::Internal(format!(
            "no evdev code for {other:?}"
        ))),
    }
}

pub(crate) fn drag_points(from: (f32, f32), to: (f32, f32), steps: u32) -> Vec<(f32, f32)> {
    let steps = steps.max(1);
    (1..=steps)
        .map(|i| {
            if i == steps {
                return to;
            }
            let t = i as f32 / steps as f32;
            (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
        })
        .collect()
}

/// The client rect a pointer action may reuse: the cached one when it belongs to `pid`.
fn cached_client(cache: &Option<(u32, Rect)>, pid: u32) -> Option<Rect> {
    cache
        .as_ref()
        .filter(|(owner, _)| *owner == pid)
        .map(|(_, client)| client.clone())
}

fn launch(stage: &'static str, message: String) -> SessionError {
    SessionError::LaunchFailed { stage, message }
}

/// Sends SIGTERM, waits up to `grace` for the exit, then SIGKILLs; reaps either way. True
/// when SIGTERM was enough.
fn terminate(child: &mut Child, grace: Duration) -> std::io::Result<bool> {
    // A reaped child's pid may already belong to someone else, so it is never signalled.
    if child.try_wait()?.is_some() {
        return Ok(true);
    }
    kill_process(Pid::from_child(child), Signal::TERM).map_err(std::io::Error::from)?;
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill()?;
    child.wait()?;
    Ok(false)
}

fn reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Polls `ready` every 100 ms until it is true or `timeout` passes; returns its last answer.
fn wait_for(
    timeout: Duration,
    mut ready: impl FnMut() -> Result<bool, SessionError>,
) -> Result<bool, SessionError> {
    let deadline = Instant::now() + timeout;
    loop {
        if ready()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(POLL);
    }
}

/// Connects to the accessibility bus and waits for the registry to own its name there.
fn connect_a11y(bus: &Connection) -> Result<Connection, SessionError> {
    let atspi = |e: zbus::Error| launch("atspi", e.to_string());
    let address: String = bus
        .call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some("org.a11y.Bus"),
            "GetAddress",
            &(),
        )
        .map_err(atspi)?
        .body()
        .deserialize()
        .map_err(atspi)?;
    let a11y = zbus::blocking::connection::Builder::address(address.as_str())
        .and_then(|builder| builder.build())
        .map_err(atspi)?;
    let owned = wait_for(START_TIMEOUT, || {
        a11y.call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "NameHasOwner",
            &(REGISTRY,),
        )
        .and_then(|reply| reply.body().deserialize::<bool>())
        .map_err(atspi)
    })?;
    if !owned {
        return Err(launch(
            "atspi",
            format!("{REGISTRY} did not appear within 5 s"),
        ));
    }
    Ok(a11y)
}

/// Picks `pid`'s window out of the geometry script's JSON.
fn parse_geometry(json: &str, pid: u32) -> Result<WindowGeometry, SessionError> {
    let bad = || SessionError::Internal(format!("unexpected geometry JSON: {json}"));
    let value: serde_json::Value = serde_json::from_str(json).map_err(|_| bad())?;
    let scale = value["scale"].as_f64().ok_or_else(bad)? as f32;
    let rect = |v: &serde_json::Value| -> Option<Rect> {
        let n: Vec<f32> = v
            .as_array()?
            .iter()
            .map(|n| n.as_f64().map(|n| n as f32))
            .collect::<Option<_>>()?;
        match n[..] {
            [x, y, w, h] => Some(Rect::new(x, y, w, h)),
            _ => None,
        }
    };
    for window in value["windows"].as_array().ok_or_else(bad)? {
        if window["pid"].as_u64() == Some(u64::from(pid)) {
            return Ok(WindowGeometry {
                frame: rect(&window["frame"]).ok_or_else(bad)?,
                client: rect(&window["client"]).ok_or_else(bad)?,
                surface: rect(&window["surface"]).ok_or_else(bad)?,
                scale,
            });
        }
    }
    Err(launch("window", format!("no window has pid {pid}")))
}

/// The highest pid above `than` in `members` owning a normal window, if any; geometry JSON as
/// `parse_geometry` reads it. A self-restart's successor is newer than the app that exited and
/// in its session; a D-Bus service's window is neither.
fn newest_window_above(
    json: &str,
    than: u32,
    members: &[u32],
) -> Result<Option<u32>, SessionError> {
    let bad = || SessionError::Internal(format!("unexpected geometry JSON: {json}"));
    let value: serde_json::Value = serde_json::from_str(json).map_err(|_| bad())?;
    let mut newest = None;
    for window in value["windows"].as_array().ok_or_else(bad)? {
        let pid = window["pid"].as_u64().ok_or_else(bad)?;
        let pid = u32::try_from(pid).map_err(|_| bad())?;
        if pid > than && members.contains(&pid) && newest.is_none_or(|newest| pid > newest) {
            newest = Some(pid);
        }
    }
    Ok(newest)
}

/// Checks a ScreenShot2 reply's geometry before any row is indexed: the stride must hold a
/// row of `width` pixels and the buffer `height` rows of `stride`. `Internal` names the values.
fn check_capture_geometry(
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
    data_len: usize,
) -> Result<(), SessionError> {
    // Every format `to_rgba` accepts is 4 bytes per pixel; an unknown one is rejected there.
    let _ = format;
    const BYTES_PER_PIXEL: u32 = 4;
    let overflow = || {
        SessionError::Internal(format!(
            "ScreenShot2 geometry {width}x{height} with stride {stride} overflows"
        ))
    };
    let row = width.checked_mul(BYTES_PER_PIXEL).ok_or_else(overflow)?;
    if stride < row {
        return Err(SessionError::Internal(format!(
            "ScreenShot2 stride {stride} is below {row} bytes for {width} pixels"
        )));
    }
    let needed = (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(overflow)?;
    if data_len < needed {
        return Err(SessionError::Internal(format!(
            "ScreenShot2 sent {data_len} bytes, need {needed} for {height} rows of {stride}"
        )));
    }
    Ok(())
}

/// Converts ScreenShot2 rows of `stride` bytes in QImage `format` into tightly packed straight
/// RGBA. Supports 6 (BGRA premultiplied), 16 (RGBX), 17 (RGBA) and 18 (RGBA premultiplied).
fn to_rgba(
    format: u32,
    data: &[u8],
    width: u32,
    height: u32,
    stride: u32,
) -> Result<Vec<u8>, SessionError> {
    // Per format: the byte order of a pixel, whether alpha is real, and whether it is premultiplied.
    let (bgr_order, has_alpha, premultiplied) = match format {
        FORMAT_ARGB32_PREMULTIPLIED => (true, true, true),
        FORMAT_RGBX8888 => (false, false, false),
        FORMAT_RGBA8888 => (false, true, false),
        FORMAT_RGBA8888_PREMULTIPLIED => (false, true, true),
        other => {
            return Err(SessionError::Internal(format!(
                "ScreenShot2 sent image format {other}, which is not one of 6, 16, 17, 18"
            )));
        }
    };
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for row in data.chunks(stride as usize).take(height as usize) {
        for &[p0, p1, p2, p3] in row[..(width * 4) as usize].as_chunks::<4>().0 {
            let (r, g, b) = if bgr_order {
                (p2, p1, p0)
            } else {
                (p0, p1, p2)
            };
            let a = if has_alpha { p3 } else { 255 };
            let straight = |c: u8| match a {
                _ if !premultiplied => c,
                0 | 255 => c,
                a => ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8,
            };
            rgba.extend_from_slice(&[straight(r), straight(g), straight(b), a]);
        }
    }
    Ok(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_pilot_protocol::Recorder;
    use gpui_pilot_protocol::Wheel;

    #[test]
    fn short_stride_is_an_internal_error_not_a_panic() {
        let err = check_capture_geometry(FORMAT_RGBA8888, 10, 2, 39, 78).unwrap_err();
        let SessionError::Internal(msg) = err else {
            panic!("expected Internal, got {err:?}");
        };
        assert!(msg.contains("39") && msg.contains("10"), "{msg}");
    }

    #[test]
    fn short_buffer_is_an_internal_error_not_a_panic() {
        let err = check_capture_geometry(FORMAT_RGBA8888, 10, 2, 40, 79).unwrap_err();
        let SessionError::Internal(msg) = err else {
            panic!("expected Internal, got {err:?}");
        };
        assert!(msg.contains("79") && msg.contains("80"), "{msg}");
    }

    #[test]
    fn padded_stride_and_exact_buffer_pass() {
        check_capture_geometry(FORMAT_RGBA8888, 10, 2, 48, 96).unwrap();
        check_capture_geometry(FORMAT_RGBA8888, 10, 2, 40, 80).unwrap();
    }

    #[test]
    fn overflowing_geometry_is_an_error() {
        assert!(check_capture_geometry(FORMAT_RGBA8888, u32::MAX, 2, u32::MAX, 0).is_err());
    }

    #[test]
    fn a_held_button_is_released_when_the_moves_fail() {
        let mut log = Vec::new();
        let result = held(
            &mut log,
            |log| {
                log.push("press");
                Ok(())
            },
            |log| {
                log.push("move");
                Err("moved off")
            },
            |log| {
                log.push("release");
                Err("release failed too")
            },
        );
        assert_eq!(result, Err("moved off"), "the first error wins");
        assert_eq!(log, ["press", "move", "release"]);

        let mut log = Vec::new();
        let result: Result<(), &str> = held(
            &mut log,
            |_| Err("no press"),
            |log| {
                log.push("move");
                Ok(())
            },
            |log| {
                log.push("release");
                Ok(())
            },
        );
        assert_eq!(result, Err("no press"));
        assert!(log.is_empty(), "nothing runs without the press: {log:?}");
    }

    #[test]
    fn drag_points_end_at_the_target_in_even_steps() {
        assert_eq!(
            drag_points((0., 0.), (100., 50.), 4),
            vec![(25., 12.5), (50., 25.), (75., 37.5), (100., 50.)]
        );
        assert_eq!(drag_points((1., 2.), (3., 4.), 0), vec![(3., 4.)]);
    }

    #[test]
    fn terminate_asks_first_and_kills_after_the_grace() {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;

        let mut polite = Command::new("sleep").arg("30").spawn().unwrap();
        assert!(
            terminate(&mut polite, Duration::from_secs(2)).unwrap(),
            "sleep exits on SIGTERM"
        );
        assert!(polite.try_wait().unwrap().is_some(), "reaped");

        // An ignored SIGTERM survives exec, so `sleep` ignores it too; `ready` follows the trap.
        let mut stubborn = Command::new("sh")
            .args(["-c", "trap '' TERM; echo ready; exec sleep 30"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(stubborn.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line, "ready\n");
        let started = Instant::now();
        assert!(
            !terminate(&mut stubborn, Duration::from_millis(200)).unwrap(),
            "only SIGKILL ends it"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(stubborn.try_wait().unwrap().is_some(), "reaped");
    }

    #[test]
    fn the_placement_script_assigns_then_calls_back_with_its_token() {
        let script =
            placement_script("gp-place-3", 129, &Rect::new(100., 60., 900., 600.), false).unwrap();
        let (assign, callback) = script.split_once(&geometry_script("gp-place-3")).unwrap();
        assert!(callback.is_empty(), "the callback comes last: {script}");
        assert!(assign.contains("w.pid == 129"), "{script}");
        assert!(
            assign.contains("w.frameGeometry = {x: 100, y: 60, width: 900, height: 600}"),
            "{script}"
        );
        assert!(!assign.contains("setMaximize"), "{script}");
    }

    #[test]
    fn a_restoring_placement_unmaximises_and_leaves_fullscreen_before_assigning() {
        let script =
            placement_script("gp-place-5", 129, &Rect::new(0., 0., 900., 600.), true).unwrap();
        let unmaximise = script.find("w.setMaximize(false, false);").expect(&script);
        let windowed = script.find("w.fullScreen = false;").expect(&script);
        let assign = script.find("w.frameGeometry =").expect(&script);
        assert!(unmaximise < assign && windowed < assign, "{script}");
    }

    #[test]
    fn the_close_script_closes_the_pids_windows_then_calls_back() {
        let script = close_script("gp-close-4", 129);
        let (close, callback) = script.split_once(&geometry_script("gp-close-4")).unwrap();
        assert!(callback.is_empty(), "the callback comes last: {script}");
        assert!(close.contains("w.pid == 129"), "{script}");
        assert!(close.contains("w.closeWindow();"), "{script}");
    }

    #[test]
    fn a_window_outside_the_apps_session_is_never_the_successor() {
        // 61 is the successor in the app's session; 86, newer, is a portal dialog D-Bus started.
        let json = r#"{"scale":1.5,"windows":[
            {"pid":61,"frame":[0,0,10,10],"client":[0,0,10,10],"surface":[0,0,10,10]},
            {"pid":86,"frame":[0,0,10,10],"client":[0,0,10,10],"surface":[0,0,10,10]}]}"#;
        assert_eq!(newest_window_above(json, 47, &[52, 61]).unwrap(), Some(61));
        assert_eq!(
            newest_window_above(json, 47, &[52]).unwrap(),
            None,
            "only the restart helper lives yet, and it has no window"
        );
    }

    #[test]
    fn the_newest_window_above_the_exited_pid_is_the_successor() {
        let json = r#"{"scale":1.5,"windows":[
            {"pid":12,"frame":[0,0,10,10],"client":[0,0,10,10],"surface":[0,0,10,10]},
            {"pid":61,"frame":[0,0,10,10],"client":[0,0,10,10],"surface":[0,0,10,10]},
            {"pid":58,"frame":[0,0,10,10],"client":[0,0,10,10],"surface":[0,0,10,10]}]}"#;
        assert_eq!(
            newest_window_above(json, 47, &[12, 58, 61]).unwrap(),
            Some(61)
        );
        assert_eq!(newest_window_above(json, 61, &[12, 58, 61]).unwrap(), None);
        assert_eq!(
            newest_window_above(r#"{"scale":1,"windows":[]}"#, 47, &[12, 58, 61]).unwrap(),
            None
        );
        assert!(newest_window_above("not json", 47, &[12, 58, 61]).is_err());
    }

    #[test]
    fn the_geometry_script_echoes_its_token() {
        assert!(geometry_script("gp-geom-7").contains(r#"token: "gp-geom-7""#));
    }

    #[test]
    fn a_non_finite_frame_is_rejected_before_any_script() {
        for frame in [
            Rect::new(f32::NAN, 0., 1., 1.),
            Rect::new(0., 0., f32::INFINITY, 1.),
        ] {
            match placement_script("gp-place-1", 1, &frame, false).unwrap_err() {
                SessionError::Internal(message) => assert!(message.contains("finite"), "{message}"),
                other => panic!("{other}"),
            }
        }
    }

    #[test]
    fn the_frames_script_hooks_one_windows_damage_and_close() {
        let script = super::frames_script("gp-frames-3", 42);
        for part in [
            "w.normalWindow && w.pid == 42",
            ".damaged.connect(",
            ".closed.connect(",
            "\"Frame\"",
            "\"FramesEnded\"",
            "\"FramesReady\"",
            "\"gp-frames-3\"",
        ] {
            assert!(script.contains(part), "{part} missing from {script}");
        }
    }

    #[test]
    fn only_the_current_recordings_token_stamps_and_ends_it() {
        let ms = Duration::from_millis;
        let recording = std::sync::Mutex::new(None);
        let t0 = Instant::now();
        super::stamp_frame(&recording, "gp-frames-1", t0);
        *recording.lock().unwrap() =
            Some(("gp-frames-2".to_owned(), Recorder::new(t0, 144_000, 100)));
        super::stamp_frame(&recording, "gp-frames-1", t0 + ms(1));
        super::stamp_frame(&recording, "gp-frames-2", t0 + ms(7));
        super::stamp_frame(&recording, "gp-frames-2", t0 + ms(14));
        super::end_frames(&recording, "gp-frames-1");
        let report = |r: &std::sync::Mutex<Option<(String, Recorder)>>| {
            r.lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .1
                .finish(t0 + ms(20), None)
        };
        let kept = report(&recording);
        assert_eq!(kept.frames, 2, "{kept:?}");
        assert!(!kept.truncated, "a stale script cannot end it");
        super::end_frames(&recording, "gp-frames-2");
        super::stamp_frame(&recording, "gp-frames-2", t0 + ms(19));
        let ended = report(&recording);
        assert_eq!(ended.frames, 2, "nothing is stamped after the end");
        assert!(ended.truncated);
    }

    #[test]
    fn only_the_awaited_scripts_callback_is_kept() {
        let mut pending = Pending::default();
        pending.expect("gp-geom-2");
        pending.offer(r#"{"token":"gp-geom-1","scale":1.5,"windows":[]}"#.into());
        pending.offer("not json".into());
        assert_eq!(
            pending.json, None,
            "a late reply from an earlier script is ignored"
        );
        let mine = r#"{"token":"gp-geom-2","scale":1.5,"windows":[]}"#;
        pending.offer(mine.into());
        pending.offer(r#"{"token":"gp-geom-1","scale":1,"windows":[]}"#.into());
        assert_eq!(pending.json.as_deref(), Some(mine));
    }

    #[test]
    fn geometry_json_yields_the_pids_window() {
        let json = r#"{"scale":1.5,"windows":[{"pid":7,"frame":[0,0,10,10],"client":[0,0,10,10],
            "surface":[0,0,10,10]},
            {"pid":129,"frame":[0,0,1536,1131.33],"client":[0,36,1536,1095.33],
            "surface":[0,36,1536,1095.33]}]}"#;
        let geometry = parse_geometry(json, 129).unwrap();
        assert_eq!(geometry.frame, Rect::new(0., 0., 1536., 1131.33));
        assert_eq!(geometry.client, Rect::new(0., 36., 1536., 1095.33));
        assert_eq!(geometry.surface, Rect::new(0., 36., 1536., 1095.33));
        assert_eq!(geometry.scale, 1.5);
        match parse_geometry(json, 5).unwrap_err() {
            SessionError::LaunchFailed { stage, .. } => assert_eq!(stage, "window"),
            other => panic!("{other}"),
        }
    }

    #[test]
    fn the_content_rect_is_the_client_seen_from_the_surface_origin() {
        // Measured on pilot-demo with client-side decorations: a 20 px shadow around the client.
        let json = r#"{"scale":1.5,"windows":[{"pid":61,"frame":[0,0,1280,800],
            "client":[0,0,1280,800],"surface":[-20,-20,1320,840]}]}"#;
        let csd = parse_geometry(json, 61).unwrap();
        assert_eq!(csd.content(), Rect::new(20., 20., 1280., 800.));
        // Server-side decorations: the surface is the client, so nothing moves.
        let json = r#"{"scale":1.5,"windows":[{"pid":61,"frame":[0,0,1280,800],
            "client":[0,36,1280,764],"surface":[0,36,1280,764]}]}"#;
        let ssd = parse_geometry(json, 61).unwrap();
        assert_eq!(ssd.content(), Rect::new(0., 0., 1280., 764.));
    }

    #[test]
    fn premultiplied_bgra_becomes_straight_rgba_without_row_padding() {
        // Two pixels per row and four bytes of padding: opaque blue, half-transparent red.
        let row = [255, 0, 0, 255, 0, 0, 64, 128, 9, 9, 9, 9];
        let rgba = to_rgba(6, &[row, row].concat(), 2, 2, 12).unwrap();
        assert_eq!(rgba.len(), 16);
        assert_eq!(rgba[..8], [0, 0, 255, 255, 128, 0, 0, 128]);
    }

    #[test]
    fn crop_scales_the_logical_rect_and_clamps_it() {
        let capture = Capture {
            width: 4,
            height: 4,
            scale: 2.,
            rgba: (0..64).collect(),
        };
        let crop = capture.crop(&Rect::new(1., 1., 5., 1.));
        assert_eq!((crop.width, crop.height), (2, 2));
        assert_eq!(crop.pixel(0, 0), capture.pixel(2, 2));
        assert_eq!(crop.pixel(1, 1), capture.pixel(3, 3));
    }

    #[test]
    fn pointer_actions_reuse_the_last_client_rect_of_the_same_pid() {
        let client = Rect::new(0., 36., 1280., 764.);
        let cache = Some((7, client.clone()));
        assert_eq!(cached_client(&cache, 7), Some(client));
        assert_eq!(
            cached_client(&cache, 8),
            None,
            "another pid reads its geometry fresh"
        );
        assert_eq!(cached_client(&None, 7), None);
    }

    #[test]
    fn notches_go_horizontal_first_one_hundred_twenty_units_each() {
        assert_eq!(
            super::notch_units(&Wheel::new(-1, 2, 16)),
            vec![(-120, 0), (0, 120), (0, 120)]
        );
        assert!(super::notch_units(&Wheel::new(0, 0, 16)).is_empty());
    }

    #[test]
    fn pacing_keeps_absolute_deadlines_after_a_stall() {
        let period = Duration::from_millis(20);
        let first = Instant::now();
        let mut scheduled = Vec::new();
        super::pace(first, period, 5, |i, at| {
            scheduled.push(at - first);
            if i == 0 {
                std::thread::sleep(Duration::from_millis(60));
            }
            Ok::<(), ()>(())
        })
        .unwrap();
        let elapsed = first.elapsed();
        assert_eq!(scheduled, (0..5).map(|i| period * i).collect::<Vec<_>>());
        // Absolute deadlines finish at the last one (80 ms); pacing after each send would take 140.
        assert!(elapsed >= Duration::from_millis(80), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(120), "{elapsed:?}");
    }

    #[test]
    fn each_supported_format_converts_one_known_pixel() {
        // Pixel (r, g, b) = (10, 20, 30) with alpha 255, laid out per the format's byte order.
        let bgra = [30, 20, 10, 255];
        let rgb_x = [10, 20, 30, 0];
        let rgba = [10, 20, 30, 255];
        for (format, pixel) in [(6, bgra), (16, rgb_x), (17, rgba), (18, rgba)] {
            let out = super::to_rgba(format, &pixel, 1, 1, 4).unwrap();
            assert_eq!(out, [10, 20, 30, 255], "format {format}");
        }
    }

    #[test]
    fn premultiplied_formats_are_made_straight_and_plain_ones_are_not() {
        // Premultiplied at alpha 128 scales each channel by 255/128 and rounds to nearest.
        let out = super::to_rgba(18, &[50, 100, 120, 128], 1, 1, 4).unwrap();
        assert_eq!(out, [100, 199, 239, 128]);
        let out = super::to_rgba(17, &[50, 100, 120, 128], 1, 1, 4).unwrap();
        assert_eq!(out, [50, 100, 120, 128]);
    }

    #[test]
    fn stride_padding_between_rows_is_skipped() {
        // Two rows of one pixel each, stride 8: the last four bytes of each row are padding.
        let data = [1, 2, 3, 0, 9, 9, 9, 9, 4, 5, 6, 0, 9, 9, 9, 9];
        let out = super::to_rgba(16, &data, 1, 2, 8).unwrap();
        assert_eq!(out, [1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn an_unknown_format_is_rejected_naming_the_number() {
        let err = super::to_rgba(5, &[0; 4], 1, 1, 4).unwrap_err();
        assert!(err.to_string().contains("image format 5"), "{err}");
    }

    #[test]
    fn pacing_stops_at_the_first_failed_send() {
        let mut sent = 0;
        let result = super::pace(Instant::now(), Duration::from_millis(1), 5, |i, _| {
            sent += 1;
            if i == 1 { Err("gone") } else { Ok(()) }
        });
        assert_eq!((result, sent), (Err("gone"), 2));
    }
}
