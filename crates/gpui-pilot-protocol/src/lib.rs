//! Wire types and JSON-lines framing between the gpui-pilot server and an app's pilot host.
//!
//! Every message is one JSON object on one line. The server sends `ClientFrame`s, the host
//! answers each with a `BackendFrame` carrying the same `id`. The server applies every default,
//! so request fields are explicit and only `Option` fields may be absent.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub mod stats;

pub use stats::{BuildInfo, FrameReport, Recorder, Spread};

/// The protocol revision both ends must agree on in `hello`.
pub const PROTOCOL_VERSION: u32 = 11;

/// The oldest host protocol the server still launches. A server at `PROTOCOL_VERSION` retries
/// a host that refuses its `Hello` at the version that host names, down to this one, and
/// refuses the requests that host would not understand (see `MODIFIERS_VERSION`).
pub const MIN_HOST_VERSION: u32 = 7;

/// The first protocol whose hosts accept `modifiers` on `click` and `drag`.
pub const MODIFIERS_VERSION: u32 = 8;

/// The first protocol whose hosts accept a `button` other than left on `drag`.
pub const DRAG_BUTTON_VERSION: u32 = 9;

/// The first protocol whose hosts accept `drop`.
pub const DROP_VERSION: u32 = 10;

/// The first protocol whose hosts accept `resize`.
pub const RESIZE_VERSION: u32 = 11;

/// What a client logs, and the server reports, in place of text typed with redaction.
pub fn redacted(text: &str) -> String {
    match text.chars().count() {
        1 => "<redacted 1 char>".to_owned(),
        n => format!("<redacted {n} chars>"),
    }
}

/// The refresh rates a launch may run at, in Hz; the pilot has measured each.
pub const REFRESH_RATES_HZ: [u32; 4] = [60, 120, 144, 240];

/// The refresh rate a launch runs at when nothing chooses one.
pub const DEFAULT_REFRESH_HZ: u32 = 144;

/// One step of an element path: a GPUI integer id or a named id.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Segment {
    Int(u64),
    Str(String),
}

/// What an input or query acts on; the JSON shape alone selects the variant.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Target {
    /// An element path: the last segment is the element's own id, earlier ones its ancestors in
    /// order, gaps allowed.
    Id { id: Vec<Segment> },
    /// The element whose own id, in its short or full form, ends with this text.
    IdSuffix { id_suffix: String },
    /// The `nth` (0-based) element with this role and exact name.
    Role {
        role: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nth: Option<u32>,
    },
    /// Logical pixels relative to the window's content.
    Point { x: f32, y: f32 },
}

/// A rectangle in logical pixels.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
}

/// One element of the app's tree as the agent sees it.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub path: Vec<Segment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub bounds: Rect,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    pub visible: bool,
    pub depth: u32,
}

impl Node {
    /// A node with every optional attribute absent.
    pub fn new(path: Vec<Segment>, bounds: Rect, visible: bool, depth: u32) -> Self {
        Self {
            path,
            id: None,
            role: None,
            name: None,
            value: None,
            bounds,
            focused: None,
            checked: None,
            selected: None,
            expanded: None,
            disabled: None,
            visible,
            depth,
        }
    }
}

/// A modifier key held during an input.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Super,
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    fn left() -> Self {
        MouseButton::Left
    }

    fn is_left(&self) -> bool {
        *self == MouseButton::Left
    }
}

/// How to answer the app's current or next file prompt.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickAnswer {
    Paths(Vec<PathBuf>),
    Cancel,
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    Open,
    Save,
}

/// What a `pick_file` answer did: answered a pending prompt, or queued for the next one.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answered {
    Open,
    Save,
    Queued,
    Cancelled,
}

/// The file prompt the app opened, as far as the host can describe it.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptInfo {
    pub kind: PromptKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_name: Option<String>,
    pub multiple: bool,
}

impl PromptInfo {
    pub fn new(kind: PromptKind, multiple: bool) -> Self {
        Self {
            kind,
            directory: None,
            suggested_name: None,
            multiple,
        }
    }
}

/// How serious a GPUI prompt is.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogLevel {
    Info,
    Warning,
    Critical,
}

/// A GPUI prompt the app is waiting on.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DialogInfo {
    pub level: DialogLevel,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub buttons: Vec<String>,
}

impl DialogInfo {
    /// A dialog without detail text.
    pub fn new(level: DialogLevel, message: impl Into<String>, buttons: Vec<String>) -> Self {
        Self {
            level,
            message: message.into(),
            detail: None,
            buttons,
        }
    }
}

/// Everything the host needs to start the app, sent in `hello`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LaunchParams {
    pub width: f32,
    pub height: f32,
    pub scale: f32,
    pub data_dir: PathBuf,
    pub args: Vec<String>,
    pub settle_horizon_ms: u64,
    /// How long the host lets the app settle before answering `hello`.
    pub settle_ms: u64,
    /// The data dir is one a previous launch kept; the app should keep its own saved state.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reused: bool,
    /// The rate the app's window refreshes at, one of `REFRESH_RATES_HZ`.
    #[serde(default = "default_refresh_hz")]
    pub refresh_hz: u32,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn default_refresh_hz() -> u32 {
    DEFAULT_REFRESH_HZ
}

impl LaunchParams {
    /// Launch parameters with the default 250 ms settle horizon, a 5000 ms launch settle and the
    /// default refresh rate.
    pub fn new(width: f32, height: f32, scale: f32, data_dir: PathBuf, args: Vec<String>) -> Self {
        Self {
            width,
            height,
            scale,
            data_dir,
            args,
            settle_horizon_ms: 250,
            settle_ms: 5000,
            reused: false,
            refresh_hz: DEFAULT_REFRESH_HZ,
        }
    }
}

/// The main window's logical size and scale factor.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub w: f32,
    pub h: f32,
    pub scale: f32,
}

impl WindowInfo {
    pub fn new(w: f32, h: f32, scale: f32) -> Self {
        Self { w, h, scale }
    }
}

/// Failure categories, serialised by variant name.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    NotRunning,
    AlreadyRunning,
    InvalidConfig,
    BuildFailed,
    LaunchFailed,
    EnvironmentMissing,
    IsolationBreach,
    ProtocolMismatch,
    ElementNotFound,
    Ambiguous,
    InvalidKeystroke,
    UnsupportedCharacter,
    Timeout,
    Unsupported,
    AppCrashed,
    Internal,
    Occluded,
    Disabled,
    InvalidArgument,
}

/// A failed request: `nearest` accompanies `ElementNotFound`, `matches` accompanies `Ambiguous`,
/// and `hit` names the element estimated to take the input when the target is `Occluded`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct PilotError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nearest: Vec<Vec<Segment>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<Node>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit: Option<Node>,
}

impl PilotError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            nearest: Vec::new(),
            matches: Vec::new(),
            hit: None,
        }
    }
}

/// What a `wait` waits for.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitUntil {
    Visible(Target),
    Gone(Target),
    Elapsed(u64),
}

/// One operation the server asks the host to perform.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Hello {
        version: u32,
        launch: LaunchParams,
    },
    Screenshot {
        target: Option<Target>,
        path: PathBuf,
    },
    Tree {
        within: Option<Target>,
        wait_for: Option<Target>,
        timeout_ms: u64,
        settle_ms: u64,
    },
    Click {
        target: Target,
        button: MouseButton,
        count: u32,
        /// Modifier keys held from before the press until after the release.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        modifiers: Vec<Modifier>,
        timeout_ms: u64,
        settle_ms: u64,
    },
    Type {
        text: String,
        target: Option<Target>,
        timeout_ms: u64,
        settle_ms: u64,
    },
    Key {
        keys: String,
        settle_ms: u64,
    },
    Scroll {
        target: Target,
        dx: f32,
        dy: f32,
        /// Modifier keys held during the wheel event.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        modifiers: Vec<Modifier>,
        /// Wheel notches sent instead of the pixel delta.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wheel: Option<Wheel>,
        timeout_ms: u64,
        settle_ms: u64,
    },
    Hover {
        target: Target,
        timeout_ms: u64,
        settle_ms: u64,
    },
    Drag {
        from: Target,
        to: Target,
        steps: u32,
        /// The button pressed at `from` and released at `to` (protocol 9). Left is left off the
        /// wire, so a left drag reads the same to a host built before this field.
        #[serde(
            default = "MouseButton::left",
            skip_serializing_if = "MouseButton::is_left"
        )]
        button: MouseButton,
        /// Modifier keys held from before the press until after the release.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        modifiers: Vec<Modifier>,
        timeout_ms: u64,
        settle_ms: u64,
    },
    /// Drag external `paths` into the window over `target` and drop them there, or with
    /// `cancel` leave the window instead (protocol 10).
    Drop {
        target: Target,
        paths: Vec<PathBuf>,
        cancel: bool,
        timeout_ms: u64,
        settle_ms: u64,
    },
    /// Resize the window's content to `width` by `height` logical pixels without moving the
    /// pointer (protocol 11).
    Resize {
        width: f32,
        height: f32,
        settle_ms: u64,
    },
    Dialog {
        button: Option<String>,
        timeout_ms: u64,
        settle_ms: u64,
    },
    Wait {
        until: WaitUntil,
        timeout_ms: u64,
    },
    PickFile {
        answer: PickAnswer,
        settle_ms: u64,
        /// How long a session waits for the dialog; only the server's session path sends it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    Clipboard {
        set: Option<String>,
        settle_ms: u64,
    },
    Close,
    /// Ask the window to close as a window manager would; the app may refuse.
    CloseWindow {
        timeout_ms: u64,
        settle_ms: u64,
    },
    /// Start recording frames; a gap longer than `idle_ms` counts as idle, not as a frame.
    FramesStart {
        idle_ms: u64,
    },
    /// Stop the recording and answer its report.
    FramesStop,
}

/// The most notches a `Wheel` may carry on either axis. Every backend and the server refuse
/// more before any input, since each notch is one event built and sent.
pub const MAX_NOTCHES: u32 = 200;

/// The gaps between notches a `Wheel` may ask for, in milliseconds. Every backend and the
/// server refuse any other gap before any input, so deadline arithmetic cannot overflow.
pub const NOTCH_MS_RANGE: std::ops::RangeInclusive<u64> = 1..=1000;

/// How long GPUI and gpui-kit wait on a hovered element before showing its tooltip.
pub const TOOLTIP_DELAY_MS: u64 = 500;

/// How long `hover` waits after its move before settling: the tooltip delay plus one frame
/// at `refresh_mhz` (rounded up), capped by `settle_ms`. A zero rate waits the whole budget.
pub fn hover_wait_ms(settle_ms: u64, refresh_mhz: u32) -> u64 {
    let frame_ms = 1_000_000u64.div_ceil(u64::from(refresh_mhz.max(1)));
    settle_ms.min(TOOLTIP_DELAY_MS + frame_ms)
}

/// The smallest and largest side `resize` accepts, in logical pixels.
pub const RESIZE_MIN: f32 = 64.0;
pub const RESIZE_MAX: f32 = 8192.0;

/// Why a `resize` is refused, unless both sides are finite and within
/// `RESIZE_MIN..=RESIZE_MAX`; every backend refuses it with `InvalidArgument` before any input.
pub fn check_resize(width: f32, height: f32) -> Result<(), String> {
    let fits = |side: f32| side.is_finite() && (RESIZE_MIN..=RESIZE_MAX).contains(&side);
    if fits(width) && fits(height) {
        return Ok(());
    }
    Err(format!(
        "`width` and `height` are logical pixels between {RESIZE_MIN} and {RESIZE_MAX}; \
         got {width}x{height}"
    ))
}

/// Wheel notches a scroll sends instead of a pixel delta: signs as `dx`/`dy`, `notch_ms` apart.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wheel {
    pub notches_x: i32,
    pub notches_y: i32,
    pub notch_ms: u64,
}

impl Wheel {
    /// Notches per axis, sent `notch_ms` apart.
    pub fn new(notches_x: i32, notches_y: i32, notch_ms: u64) -> Self {
        Self {
            notches_x,
            notches_y,
            notch_ms,
        }
    }
}

/// The successful result of a `Request`; `Closed` answers `Close`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Reply {
    Hello {
        version: u32,
        pid: u32,
        window: WindowInfo,
        settled: bool,
        /// The refresh rate the app's output really has, in millihertz.
        refresh_mhz: u32,
    },
    Screenshot {
        w: u32,
        h: u32,
        scale: f32,
    },
    Tree {
        nodes: Vec<Node>,
        /// The file dialog the app is waiting on, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<PromptInfo>,
        /// An answer is queued for the next dialog the app opens.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pick_queued: bool,
        /// The GPUI prompt the app is waiting on, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dialog: Option<DialogInfo>,
    },
    Click {
        clicked: Node,
        hit: Node,
        settled: bool,
    },
    Type {
        settled: bool,
        /// The focused input hid its text (a masked or password input). Hosts before this
        /// field omit it, so it reads as false and only an explicit redaction applies.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        masked: bool,
    },
    Key {
        settled: bool,
    },
    Scroll {
        settled: bool,
    },
    Hover {
        hovered: Node,
        hit: Node,
        settled: bool,
    },
    Drag {
        from: Node,
        to: Node,
        settled: bool,
    },
    /// `dropped` is false when the drag left the window instead of dropping.
    Drop {
        over: Node,
        hit: Node,
        dropped: bool,
        settled: bool,
    },
    /// The size the window really has after a `Resize`.
    Resize {
        window: WindowInfo,
        settled: bool,
    },
    Dialog {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dialog: Option<DialogInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answered: Option<String>,
        settled: bool,
    },
    Wait {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<Node>,
        waited_ms: u64,
    },
    PickFile {
        answered: Answered,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<PromptInfo>,
        settled: bool,
    },
    Clipboard {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        settled: bool,
    },
    Closed,
    CloseWindow {
        closed: bool,
        settled: bool,
    },
    /// A recording started on a display refreshing at `refresh_mhz`.
    FramesStarted {
        refresh_mhz: u32,
    },
    /// The report of a stopped recording.
    Frames(FrameReport),
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok(Reply),
    Err(PilotError),
}

/// A request on its way to the host, numbered so the answer can be paired with it.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientFrame {
    pub id: u64,
    pub request: Request,
}

impl ClientFrame {
    pub fn new(id: u64, request: Request) -> Self {
        Self { id, request }
    }
}

/// The host's answer to the `ClientFrame` with the same `id`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackendFrame {
    pub id: u64,
    pub outcome: Outcome,
}

impl BackendFrame {
    pub fn new(id: u64, outcome: Outcome) -> Self {
        Self { id, outcome }
    }
}

/// A framing failure. JSON errors travel as text so no `serde_json` type is public.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(String),
}

/// Writes one JSON object and a newline, then flushes.
pub fn write_line<T: Serialize>(out: &mut impl Write, message: &T) -> Result<(), FrameError> {
    let mut line = serde_json::to_vec(message).map_err(|e| FrameError::Json(e.to_string()))?;
    line.push(b'\n');
    out.write_all(&line)?;
    out.flush()?;
    Ok(())
}

/// A line that parsed as JSON text but not as the expected frame, with the `id` it carried.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub struct BadFrame {
    pub id: Option<u64>,
    pub message: String,
}

/// As `read_line`, but a line that is not a `T` is `Ok(Some(Err(BadFrame)))` so the reader can
/// answer it and carry on; only i/o failures are errors. A host uses it so a request from a
/// newer server fails cleanly instead of ending the connection.
pub fn read_line_lenient<T: DeserializeOwned>(
    input: &mut impl BufRead,
) -> Result<Option<Result<T, BadFrame>>, FrameError> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&line).map_err(|error| {
        BadFrame {
            id: serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .and_then(|value| value.get("id")?.as_u64()),
            message: error.to_string(),
        }
    })))
}

/// Reads one line; `Ok(None)` at end of stream.
pub fn read_line<T: DeserializeOwned>(input: &mut impl BufRead) -> Result<Option<T>, FrameError> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    serde_json::from_str(&line)
        .map(Some)
        .map_err(|e| FrameError::Json(e.to_string()))
}
