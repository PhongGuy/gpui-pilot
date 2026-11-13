//! Tool parameter types, their defaults, and the mapping to protocol requests and results.

use crate::bound::MAX_SLICE_CHARS;
use crate::config::Mode;
use crate::error::ToolError;
use gpui_pilot_protocol::{
    ErrorCode, MAX_NOTCHES, Modifier, MouseButton, NOTCH_MS_RANGE, PickAnswer, Reply, Request,
    Segment, Target, WaitUntil, Wheel,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where the app's network access goes: headless runs on the `host` network, session on
/// `none` or `pasta`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Network {
    None,
    Pasta,
    Host,
}

/// A mouse button.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Button {
    #[default]
    Left,
    Right,
    Middle,
}

impl From<Button> for MouseButton {
    fn from(button: Button) -> Self {
        match button {
            Button::Left => MouseButton::Left,
            Button::Right => MouseButton::Right,
            Button::Middle => MouseButton::Middle,
        }
    }
}

/// A modifier key held during a click, drag or scroll, named as `key` names them.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModifierArg {
    Ctrl,
    Alt,
    Shift,
    #[serde(alias = "cmd", alias = "win", alias = "platform")]
    Super,
}

impl From<ModifierArg> for Modifier {
    fn from(modifier: ModifierArg) -> Self {
        match modifier {
            ModifierArg::Ctrl => Modifier::Ctrl,
            ModifierArg::Alt => Modifier::Alt,
            ModifierArg::Shift => Modifier::Shift,
            ModifierArg::Super => Modifier::Super,
        }
    }
}

/// How `close` ends the app: `kill` ends it outright, `window` asks its window to close as
/// the window manager would, so the app may refuse.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CloseMode {
    #[default]
    Kill,
    Window,
}

/// One step of an element path: a number or a name.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum SegmentArg {
    Int(u64),
    Str(String),
}

/// Every target shape, for an error that tells the agent what it should have sent.
const TARGET_SHAPES: &str = r#"a target is one of {"id": [segment, ...]}, {"id_suffix": "text"}, {"role": "Button", "name": "text", "nth"?: 0} or {"x": number, "y": number}, where a segment is a string or a non-negative integer"#;

/// What to act on: an element path, an id ending, a role and name, or a point in logical
/// pixels. The schema derives from the `serde` attributes; `Deserialize` is hand-written so a
/// miss lists the accepted shapes instead of serde's untagged message.
#[derive(Clone, Debug, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum TargetArg {
    /// An element path: the last segment is the element's own id, earlier ones its ancestors in
    /// order, gaps allowed.
    Id { id: Vec<SegmentArg> },
    /// An element whose own id ends with this text.
    IdSuffix { id_suffix: String },
    /// The `nth` (0-based) element with this role and exact name.
    Role {
        role: String,
        name: String,
        nth: Option<u32>,
    },
    /// Logical pixels relative to the window's content.
    Point { x: f32, y: f32 },
}

/// The derived twin of `TargetArg` that does the actual parsing.
#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum Shape {
    Id {
        id: Vec<SegmentArg>,
    },
    IdSuffix {
        id_suffix: String,
    },
    Role {
        role: String,
        name: String,
        nth: Option<u32>,
    },
    Point {
        x: f32,
        y: f32,
    },
}

impl<'de> Deserialize<'de> for TargetArg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match Shape::deserialize(&value) {
            Ok(Shape::Id { id }) => Ok(TargetArg::Id { id }),
            Ok(Shape::IdSuffix { id_suffix }) => Ok(TargetArg::IdSuffix { id_suffix }),
            Ok(Shape::Role { role, name, nth }) => Ok(TargetArg::Role { role, name, nth }),
            Ok(Shape::Point { x, y }) => Ok(TargetArg::Point { x, y }),
            Err(_) => {
                let sent: String = value.to_string().chars().take(200).collect();
                Err(serde::de::Error::custom(format!(
                    "invalid target {sent}: {TARGET_SHAPES}"
                )))
            }
        }
    }
}

impl From<TargetArg> for Target {
    fn from(target: TargetArg) -> Self {
        match target {
            TargetArg::Id { id } => Target::Id {
                id: id
                    .into_iter()
                    .map(|s| match s {
                        SegmentArg::Int(n) => Segment::Int(n),
                        SegmentArg::Str(s) => Segment::Str(s),
                    })
                    .collect(),
            },
            TargetArg::IdSuffix { id_suffix } => Target::IdSuffix { id_suffix },
            TargetArg::Role { role, name, nth } => Target::Role { role, name, nth },
            TargetArg::Point { x, y } => Target::Point { x, y },
        }
    }
}

fn settle_ms() -> u64 {
    2000
}
fn width() -> f32 {
    1280.0
}
fn height() -> f32 {
    800.0
}
fn scale() -> f32 {
    1.0
}
fn one() -> u32 {
    1
}
fn yes() -> bool {
    true
}
fn tree_timeout_ms() -> u64 {
    5000
}
fn pick_timeout_ms() -> u64 {
    10_000
}
fn drag_steps() -> u32 {
    10
}
fn action_timeout_ms() -> u64 {
    5000
}
fn tree_settle_ms() -> u64 {
    1000
}
fn launch_settle_ms() -> u64 {
    5000
}
fn close_timeout_ms() -> u64 {
    2000
}

/// The default gap between notches, one 60 Hz frame.
const NOTCH_MS: u64 = 16;
/// The default and largest interval `frames` treats as busy rather than idle.
const IDLE_MS: u64 = 100;
const MAX_IDLE_MS: u64 = 10_000;

/// Arguments of `launch`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchArgs {
    /// `headless` or `session`.
    pub mode: Mode,
    /// Window width in logical pixels.
    #[serde(default = "width")]
    pub width: f32,
    /// Window height in logical pixels.
    #[serde(default = "height")]
    pub height: f32,
    /// Display scale factor.
    #[serde(default = "scale")]
    pub scale: f32,
    /// Extra arguments for the app.
    #[serde(default)]
    pub args: Vec<String>,
    /// Network access for the app: `host` in headless mode (the default there), `none` or
    /// `pasta` in session mode (default: the config's `[session] network`, else `none`).
    pub network: Option<Network>,
    /// Session with `pasta` only: host loopback TCP ports the app reaches at 127.0.0.1.
    #[serde(default)]
    pub host_ports: Vec<u16>,
    /// Run the configured build command first.
    #[serde(default = "yes")]
    pub build: bool,
    /// Close a running app instead of failing with `AlreadyRunning`.
    #[serde(default)]
    pub replace: bool,
    /// How long to let the app settle before answering, at most 20000.
    #[serde(default = "launch_settle_ms")]
    pub settle_ms: u64,
    /// Display refresh rate in Hz: 60, 120, 144 or 240 (default: the mode's `refresh_hz` in
    /// gpui-pilot.toml, else 144).
    pub refresh_hz: Option<u32>,
    /// Extra environment variables for the app.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Start on the data dir and HOME a previous `close` with `keep_data` kept, in the same
    /// mode. With `replace`, the running app is closed keeping its data first.
    #[serde(default)]
    pub keep_data: bool,
    /// Absolute path of a git worktree of the server's project to launch from instead of the
    /// server's own folder: its gpui-pilot.toml, build, binary and binds are used, and relative
    /// `pick_file` and `save_to` paths resolve against it until the app closes. A worktree
    /// builds into its own target/, so its first build is cold.
    pub project_dir: Option<String>,
}

/// Arguments of `close`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CloseArgs {
    /// `kill` (the default) or `window`.
    #[serde(default)]
    pub mode: CloseMode,
    /// Keep the data dir and HOME for a later `launch` with `keep_data`.
    #[serde(default)]
    pub keep_data: bool,
    /// Window mode: how long to wait for the window to go.
    #[serde(default = "close_timeout_ms")]
    pub timeout_ms: u64,
    /// Window mode: how long to let a refusing app settle afterwards.
    #[serde(default = "settle_ms")]
    pub settle_ms: u64,
}

/// Arguments of `screenshot`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScreenshotArgs {
    /// Crop to this element; the whole window when absent.
    pub target: Option<TargetArg>,
    /// Save the PNG here instead of a private temporary file.
    pub save_to: Option<PathBuf>,
}

/// Arguments of `tree`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TreeArgs {
    /// Only the subtree under this element.
    pub within: Option<TargetArg>,
    /// Wait until this element exists before answering.
    pub wait_for: Option<TargetArg>,
    /// How long `wait_for` may wait.
    #[serde(default = "tree_timeout_ms")]
    pub timeout_ms: u64,
    /// How long to let the app settle before reading the tree.
    #[serde(default = "tree_settle_ms")]
    pub settle_ms: u64,
    /// Report this slice of the `within` element's value instead of the capped one (values
    /// over 2000 chars are otherwise cut to their first and last 400). Offsets count chars of
    /// the reported text, after any `<redacted N chars>` placeholder; a root without a value
    /// comes back unchanged, with no `value_*` fields.
    pub value_slice: Option<ValueSlice>,
    /// Shorten a reply longer than this many characters of JSON: runs of 4 or more similar
    /// siblings leave `nodes` and are summarised in a top-level `collapsed` array. Absent, the
    /// reply is never shortened.
    pub max_chars: Option<usize>,
}

/// A slice of an element's value, in characters.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValueSlice {
    pub offset: u64,
    /// At most 65536.
    pub length: u64,
}

/// Arguments of `click`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClickArgs {
    /// The element or point to click.
    pub target: TargetArg,
    /// Which mouse button to press.
    #[serde(default)]
    pub button: Button,
    /// Number of clicks, for double or triple clicks.
    #[serde(default = "one")]
    pub count: u32,
    /// Modifier keys held during the click: `ctrl`, `alt`, `shift`, `super` (or `cmd`).
    #[serde(default)]
    pub modifiers: Vec<ModifierArg>,
    /// How long to wait for the target to be actionable.
    #[serde(default = "action_timeout_ms")]
    pub timeout_ms: u64,
    /// How long to wait for the app to settle afterwards.
    #[serde(default = "settle_ms")]
    pub settle_ms: u64,
}

/// Arguments of `type`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypeArgs {
    pub text: String,
    /// Click this element first to focus it.
    pub target: Option<TargetArg>,
    /// Report the text only as `<redacted N chars>`, for secrets in inputs that are not
    /// masked. Text typed into a masked or password input is redacted without it (headless).
    #[serde(default)]
    pub redact: bool,
    /// How long to wait for the target to be actionable.
    #[serde(default = "action_timeout_ms")]
    pub timeout_ms: u64,
    /// How long to wait for the app to settle afterwards.
    #[serde(default = "settle_ms")]
    pub settle_ms: u64,
}

/// Arguments of `key`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyArgs {
    /// A GPUI keystroke such as `cmd-s` or `enter`.
    pub keys: String,
    /// How long to wait for the app to settle afterwards.
    #[serde(default = "settle_ms")]
    pub settle_ms: u64,
}

/// Arguments of `scroll`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScrollArgs {
    /// The element or point to scroll over.
    pub target: TargetArg,
    /// Horizontal scroll in logical pixels. Exact headless; in session mode it is sent as wheel
    /// lines assuming 20 px lines, so the distance scales with the element's line height.
    #[serde(default)]
    pub dx: Option<f32>,
    /// Vertical scroll in logical pixels, with the same session-mode caveat as `dx`.
    #[serde(default)]
    pub dy: Option<f32>,
    /// Vertical wheel notches, positive down like `dy`; at most 200 either way.
    #[serde(default)]
    pub notches: Option<i32>,
    /// Horizontal wheel notches, positive right like `dx`; at most 200 either way.
    #[serde(default)]
    pub notches_x: Option<i32>,
    /// Milliseconds between notches (default 16, 1 to 1000).
    #[serde(default)]
    pub notch_ms: Option<u64>,
    /// Modifier keys held during the scroll: `ctrl`, `alt`, `shift`, `super` (or `cmd`).
    #[serde(default)]
    pub modifiers: Vec<ModifierArg>,
    /// How long to wait for the target to be actionable.
    #[serde(default = "action_timeout_ms")]
    pub timeout_ms: u64,
    /// How long to wait for the app to settle afterwards.
    #[serde(default = "settle_ms")]
    pub settle_ms: u64,
}

/// What `frames` does: begin a recording or end it with its report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FramesAction {
    Start,
    Stop,
}

/// Arguments of `frames`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FramesArgs {
    /// `start` or `stop`.
    pub action: FramesAction,
    /// Intervals longer than this are idle gaps (default 100, 0 to 10000); start only.
    #[serde(default)]
    pub idle_ms: Option<u64>,
}

/// The longest `wait` for a fixed duration, so the call stays well inside a client's patience.
pub(crate) const MAX_WAIT_MS: u64 = 30_000;

/// Arguments of `hover`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HoverArgs {
    /// The element or point to move the pointer over.
    pub target: TargetArg,
    /// How long to wait for the target to be actionable.
    #[serde(default = "action_timeout_ms")]
    pub timeout_ms: u64,
    /// How long to wait for the app to settle afterwards.
    #[serde(default = "settle_ms")]
    pub settle_ms: u64,
}

/// Arguments of `drag`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DragArgs {
    /// The element or point to press on.
    pub from: TargetArg,
    /// The element or point to release on.
    pub to: TargetArg,
    /// How many pointer moves the drag takes, 1 to 100.
    #[serde(default = "drag_steps")]
    pub steps: u32,
    /// Which mouse button to hold, `left` unless set (`right` or `middle` to pan, say).
    #[serde(default)]
    pub button: Button,
    /// Modifier keys held during the drag: `ctrl`, `alt`, `shift`, `super` (or `cmd`).
    #[serde(default)]
    pub modifiers: Vec<ModifierArg>,
    /// How long to wait for both ends to be actionable.
    #[serde(default = "action_timeout_ms")]
    pub timeout_ms: u64,
    /// How long to wait for the app to settle afterwards.
    #[serde(default = "settle_ms")]
    pub settle_ms: u64,
}

/// Arguments of `drop`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DropArgs {
    /// The element or point to drop on.
    pub target: TargetArg,
    /// Absolute paths of the files and folders dropped; they need not exist.
    pub paths: Vec<PathBuf>,
    /// Leave the window instead of dropping, as a cancelled drag does.
    #[serde(default)]
    pub cancel: bool,
    /// How long to wait for the target to be actionable.
    #[serde(default = "action_timeout_ms")]
    pub timeout_ms: u64,
    /// How long to wait for the app to settle afterwards.
    #[serde(default = "settle_ms")]
    pub settle_ms: u64,
}

/// Arguments of `resize`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResizeArgs {
    /// The window content's new width in logical pixels.
    pub width: f32,
    /// The window content's new height in logical pixels.
    pub height: f32,
    /// How long to wait for the app to settle afterwards.
    #[serde(default = "settle_ms")]
    pub settle_ms: u64,
}

/// Arguments of `dialog`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DialogArgs {
    /// Answer with the button that has exactly this label; read the dialog when absent.
    pub button: Option<String>,
    /// With `button`, how long to wait for a dialog to appear; ignored when reading.
    #[serde(default = "action_timeout_ms")]
    pub timeout_ms: u64,
    /// How long to wait for the app to settle afterwards.
    #[serde(default = "settle_ms")]
    pub settle_ms: u64,
}

/// Arguments of `wait`: exactly one of `target` and `ms`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitArgs {
    /// Wait until this element is visible, or with `gone`, until no visible element matches.
    pub target: Option<TargetArg>,
    /// Wait for `target` to disappear instead of appear.
    #[serde(default)]
    pub gone: bool,
    /// Wait this many milliseconds instead, at most 30000.
    pub ms: Option<u64>,
    /// How long `target` may take.
    #[serde(default = "action_timeout_ms")]
    pub timeout_ms: u64,
}

/// Arguments of `pick_file`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PickFileArgs {
    /// Answer the prompt with these paths (exactly one of `paths` and `cancel`).
    pub paths: Option<Vec<PathBuf>>,
    /// Cancel the prompt instead.
    #[serde(default)]
    pub cancel: bool,
    /// Accepted for session mode; headless answers or queues at once.
    #[serde(default = "pick_timeout_ms")]
    pub timeout_ms: u64,
    /// How long to wait for the app to settle afterwards.
    #[serde(default = "settle_ms")]
    pub settle_ms: u64,
}

/// Arguments of `clipboard`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClipboardArgs {
    /// Replace the clipboard text; read it when absent.
    pub set: Option<String>,
    /// How long to wait for the app to settle afterwards.
    #[serde(default = "settle_ms")]
    pub settle_ms: u64,
}

impl TreeArgs {
    /// The requested value slice; one without `within`, or longer than the slice limit, is
    /// `InvalidArgument`.
    pub fn slice(&self) -> Result<Option<ValueSlice>, ToolError> {
        let invalid = |message: &str| Err(ToolError::new(ErrorCode::InvalidArgument, message));
        match self.value_slice {
            Some(_) if self.within.is_none() => {
                invalid("`value_slice` needs `within` naming the element whose value to slice")
            }
            Some(slice) if slice.length > MAX_SLICE_CHARS => invalid(&format!(
                "`value_slice.length` may be at most {MAX_SLICE_CHARS} chars"
            )),
            slice => Ok(slice),
        }
    }

    pub fn request(self) -> Request {
        Request::Tree {
            within: self.within.map(Target::from),
            wait_for: self.wait_for.map(Target::from),
            timeout_ms: self.timeout_ms,
            settle_ms: self.settle_ms,
        }
    }
}

impl ClickArgs {
    pub fn request(self) -> Request {
        Request::Click {
            target: self.target.into(),
            button: self.button.into(),
            count: self.count,
            modifiers: self.modifiers.into_iter().map(Modifier::from).collect(),
            timeout_ms: self.timeout_ms,
            settle_ms: self.settle_ms,
        }
    }
}

impl TypeArgs {
    pub fn request(self) -> Request {
        Request::Type {
            text: self.text,
            target: self.target.map(Target::from),
            timeout_ms: self.timeout_ms,
            settle_ms: self.settle_ms,
        }
    }
}

impl KeyArgs {
    pub fn request(self) -> Request {
        Request::Key {
            keys: self.keys,
            settle_ms: self.settle_ms,
        }
    }
}

impl ScrollArgs {
    /// Pixels (`dx`, `dy`) or notches (`notches`, `notches_x`, `notch_ms`), never both and
    /// never neither; notch counts and gaps outside their limits are `InvalidArgument`.
    pub fn request(self) -> Result<Request, ToolError> {
        let invalid = |message: &str| Err(ToolError::new(ErrorCode::InvalidArgument, message));
        let pixels = self.dx.is_some() || self.dy.is_some();
        let notched = self.notches.is_some() || self.notches_x.is_some();
        let wheel = match (pixels, notched) {
            (false, false) => return invalid("pass `dx`/`dy` pixels or `notches`/`notches_x`"),
            (true, true) => return invalid("pass pixels or notches, not both"),
            (true, false) if self.notch_ms.is_some() => {
                return invalid("`notch_ms` needs `notches` or `notches_x`");
            }
            (true, false) => None,
            (false, true) => {
                let (x, y) = (self.notches_x.unwrap_or(0), self.notches.unwrap_or(0));
                if x.unsigned_abs() > MAX_NOTCHES || y.unsigned_abs() > MAX_NOTCHES {
                    return invalid(&format!("at most {MAX_NOTCHES} notches per axis"));
                }
                if x == 0 && y == 0 {
                    return invalid("send at least one notch");
                }
                let notch_ms = self.notch_ms.unwrap_or(NOTCH_MS);
                if !NOTCH_MS_RANGE.contains(&notch_ms) {
                    return invalid("`notch_ms` is between 1 and 1000");
                }
                Some(Wheel::new(x, y, notch_ms))
            }
        };
        Ok(Request::Scroll {
            target: self.target.into(),
            dx: self.dx.unwrap_or(0.0),
            dy: self.dy.unwrap_or(0.0),
            modifiers: self.modifiers.into_iter().map(Modifier::from).collect(),
            wheel,
            timeout_ms: self.timeout_ms,
            settle_ms: self.settle_ms,
        })
    }

    /// How long the notches take to send: every notch of both axes, `notch_ms` apart.
    pub fn notch_time_ms(&self) -> u64 {
        let count = self.notches.unwrap_or(0).unsigned_abs() as u64
            + self.notches_x.unwrap_or(0).unsigned_abs() as u64;
        count.saturating_mul(self.notch_ms.unwrap_or(NOTCH_MS))
    }
}

impl FramesArgs {
    /// `start` with `idle_ms` up to 10000 (default 100); `stop` takes no `idle_ms`.
    pub fn request(self) -> Result<Request, ToolError> {
        let invalid = |message: &str| Err(ToolError::new(ErrorCode::InvalidArgument, message));
        match (self.action, self.idle_ms) {
            (FramesAction::Start, Some(idle_ms)) if idle_ms > MAX_IDLE_MS => {
                invalid("`idle_ms` is at most 10000")
            }
            (FramesAction::Start, idle_ms) => Ok(Request::FramesStart {
                idle_ms: idle_ms.unwrap_or(IDLE_MS),
            }),
            (FramesAction::Stop, Some(_)) => invalid("`idle_ms` applies to `start` only"),
            (FramesAction::Stop, None) => Ok(Request::FramesStop),
        }
    }
}

impl CloseArgs {
    /// The `CloseWindow` request a window-mode close sends.
    pub fn window_request(&self) -> Request {
        Request::CloseWindow {
            timeout_ms: self.timeout_ms,
            settle_ms: self.settle_ms,
        }
    }
}

impl HoverArgs {
    pub fn request(self) -> Request {
        Request::Hover {
            target: self.target.into(),
            timeout_ms: self.timeout_ms,
            settle_ms: self.settle_ms,
        }
    }
}

impl DragArgs {
    /// `steps` outside 1 to 100 is `InvalidArgument`.
    pub fn request(self) -> Result<Request, ToolError> {
        if !(1..=100).contains(&self.steps) {
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "`steps` is between 1 and 100",
            ));
        }
        Ok(Request::Drag {
            from: self.from.into(),
            to: self.to.into(),
            steps: self.steps,
            button: self.button.into(),
            modifiers: self.modifiers.into_iter().map(Modifier::from).collect(),
            timeout_ms: self.timeout_ms,
            settle_ms: self.settle_ms,
        })
    }
}

impl DropArgs {
    /// No paths, or any relative one, is `InvalidArgument`.
    pub fn request(self) -> Result<Request, ToolError> {
        if self.paths.is_empty() || self.paths.iter().any(|path| !path.is_absolute()) {
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                "`paths` is a non-empty list of absolute paths",
            ));
        }
        Ok(Request::Drop {
            target: self.target.into(),
            paths: self.paths,
            cancel: self.cancel,
            timeout_ms: self.timeout_ms,
            settle_ms: self.settle_ms,
        })
    }
}

impl ResizeArgs {
    /// A side that is not finite or lies outside 64..=8192 is `InvalidArgument`.
    pub fn request(self) -> Result<Request, ToolError> {
        gpui_pilot_protocol::check_resize(self.width, self.height)
            .map_err(|message| ToolError::new(ErrorCode::InvalidArgument, message))?;
        Ok(Request::Resize {
            width: self.width,
            height: self.height,
            settle_ms: self.settle_ms,
        })
    }
}

impl DialogArgs {
    pub fn request(self) -> Request {
        Request::Dialog {
            button: self.button,
            timeout_ms: self.timeout_ms,
            settle_ms: self.settle_ms,
        }
    }
}

impl WaitArgs {
    /// `ms` alone sleeps; an element `target` alone waits for it to appear or, with `gone`,
    /// disappear. Any other combination is `InvalidArgument`.
    pub fn request(self) -> Result<Request, ToolError> {
        let invalid = |message: &str| ToolError::new(ErrorCode::InvalidArgument, message);
        let (until, timeout_ms) = match (self.target, self.ms) {
            (None, Some(_)) if self.gone => return Err(invalid("`gone` needs a `target`")),
            (None, Some(ms)) if ms > MAX_WAIT_MS => {
                return Err(invalid("`ms` is at most 30000"));
            }
            (None, Some(ms)) => (WaitUntil::Elapsed(ms), ms),
            (Some(TargetArg::Point { .. }), None) => {
                return Err(invalid("`target` must be an element, not a point"));
            }
            (Some(target), None) if self.gone => (WaitUntil::Gone(target.into()), self.timeout_ms),
            (Some(target), None) => (WaitUntil::Visible(target.into()), self.timeout_ms),
            _ => return Err(invalid("pass exactly one of `target` and `ms`")),
        };
        Ok(Request::Wait { until, timeout_ms })
    }
}

impl ClipboardArgs {
    pub fn request(self) -> Request {
        Request::Clipboard {
            set: self.set,
            settle_ms: self.settle_ms,
        }
    }
}

impl PickFileArgs {
    /// Exactly one of non-empty `paths` and `cancel: true`; the `Err` text is for the client.
    /// Paths stay as given: the driver resolves relative ones against the project folder.
    pub fn request(self) -> Result<Request, String> {
        let answer = match (self.paths.filter(|p| !p.is_empty()), self.cancel) {
            (Some(paths), false) => PickAnswer::Paths(paths),
            (None, true) => PickAnswer::Cancel,
            _ => return Err("pass exactly one of non-empty `paths` and `cancel: true`".into()),
        };
        Ok(Request::PickFile {
            answer,
            settle_ms: self.settle_ms,
            timeout_ms: Some(self.timeout_ms),
        })
    }
}

/// `path` resolved against `root` when relative.
pub(crate) fn absolute(root: &Path, path: &Path) -> PathBuf {
    root.join(path)
}

/// A reply as a JSON object without the `op` tag and without top-level nulls.
pub(crate) fn structured(reply: &Reply) -> Value {
    let mut value = serde_json::to_value(reply).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.remove("op");
        map.retain(|_, v| !v.is_null());
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_pilot_protocol::{Answered, Modifier, Node, Rect, Wheel};
    use serde_json::json;

    #[test]
    fn a_tree_is_shortened_only_when_max_chars_is_given() {
        let max = |value: Value| args::<TreeArgs>(value).unwrap().max_chars;
        assert_eq!(max(json!({})), None);
        assert_eq!(max(json!({"max_chars": 30000})), Some(30000));
    }

    fn args<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, serde_json::Error> {
        serde_json::from_value(value)
    }

    #[test]
    fn a_value_slice_needs_within_and_a_bounded_length() {
        let slice = |value: Value| args::<TreeArgs>(value).unwrap().slice();
        let within = json!({"id": ["big"]});
        let ok = slice(json!({"within": within, "value_slice": {"offset": 5, "length": 65_536}}));
        let ok = ok.unwrap().unwrap();
        assert_eq!((ok.offset, ok.length), (5, 65_536));
        assert!(slice(json!({})).unwrap().is_none());
        let alone = slice(json!({"value_slice": {"offset": 0, "length": 1}})).unwrap_err();
        assert_eq!(alone.code(), ErrorCode::InvalidArgument);
        let long = json!({"within": within, "value_slice": {"offset": 0, "length": 65_537}});
        assert_eq!(slice(long).unwrap_err().code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn type_redacts_only_on_request() {
        let plain: TypeArgs = args(json!({"text": "abc"})).unwrap();
        assert!(!plain.redact);
        let secret: TypeArgs = args(json!({"text": "abc", "redact": true})).unwrap();
        assert!(secret.redact);
    }

    #[test]
    fn defaults_fill_every_optional_parameter() {
        let click: ClickArgs = args(json!({"target": {"id": ["count"]}})).unwrap();
        assert_eq!(
            click.request(),
            Request::Click {
                target: Target::Id {
                    id: vec![Segment::Str("count".into())]
                },
                button: MouseButton::Left,
                count: 1,
                modifiers: Vec::new(),
                timeout_ms: 5000,
                settle_ms: 2000,
            }
        );
        let launch: LaunchArgs = args(json!({"mode": "headless"})).unwrap();
        assert_eq!(
            (launch.width, launch.height, launch.scale),
            (1280.0, 800.0, 1.0)
        );
        assert!(launch.build && !launch.replace && launch.args.is_empty());
        assert_eq!(launch.network, None);
        assert_eq!(launch.settle_ms, 5000);
        assert_eq!(launch.refresh_hz, None);
        assert_eq!(launch.project_dir, None);
        let tree: TreeArgs = args(json!({})).unwrap();
        assert_eq!(
            tree.request(),
            Request::Tree {
                within: None,
                wait_for: None,
                timeout_ms: 5000,
                settle_ms: 1000,
            }
        );
        let scroll: ScrollArgs = args(json!({"target": {"x": 5, "y": 6}, "dy": 120})).unwrap();
        assert_eq!(
            scroll.request().unwrap(),
            Request::Scroll {
                target: Target::Point { x: 5.0, y: 6.0 },
                dx: 0.0,
                dy: 120.0,
                modifiers: Vec::new(),
                wheel: None,
                timeout_ms: 5000,
                settle_ms: 2000
            }
        );
    }

    #[test]
    fn click_and_drag_modifiers_parse_like_scrolls_and_default_to_none() {
        let click: ClickArgs =
            args(json!({"target": {"x": 5, "y": 6}, "modifiers": ["shift", "cmd"]})).unwrap();
        let Request::Click { modifiers, .. } = click.request() else {
            panic!("not a click")
        };
        assert_eq!(modifiers, [Modifier::Shift, Modifier::Super]);
        let drag: DragArgs =
            args(json!({"from": {"x": 1, "y": 2}, "to": {"x": 3, "y": 4}, "modifiers": ["alt"]}))
                .unwrap();
        let Request::Drag { modifiers, .. } = drag.request().unwrap() else {
            panic!("not a drag")
        };
        assert_eq!(modifiers, [Modifier::Alt]);
        // An unknown name is refused exactly as `scroll` refuses it.
        assert!(
            args::<ClickArgs>(json!({"target": {"x": 5, "y": 6}, "modifiers": ["hyper"]})).is_err()
        );
        assert!(
            args::<DragArgs>(
                json!({"from": {"x": 1, "y": 2}, "to": {"x": 3, "y": 4}, "modifiers": ["hyper"]})
            )
            .is_err()
        );
        let plain: ClickArgs = args(json!({"target": {"x": 5, "y": 6}})).unwrap();
        let Request::Click { modifiers, .. } = plain.request() else {
            panic!("not a click")
        };
        assert!(modifiers.is_empty());
    }

    #[test]
    fn a_drag_button_parses_like_clicks_and_defaults_to_left() {
        let button = |extra: Value| {
            let mut value = json!({"from": {"x": 1, "y": 2}, "to": {"x": 3, "y": 4}});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let drag: DragArgs = args(value)?;
            let Request::Drag { button, .. } = drag.request().unwrap() else {
                panic!("not a drag")
            };
            Ok::<_, serde_json::Error>(button)
        };
        assert_eq!(button(json!({})).unwrap(), MouseButton::Left);
        assert_eq!(
            button(json!({"button": "right"})).unwrap(),
            MouseButton::Right
        );
        assert_eq!(
            button(json!({"button": "middle"})).unwrap(),
            MouseButton::Middle
        );
        assert!(button(json!({"button": "back"})).is_err());
    }

    #[test]
    fn a_resize_takes_a_finite_size_between_64_and_8192() {
        let resize = |value: Value| args::<ResizeArgs>(value).unwrap().request();
        let Request::Resize {
            width,
            height,
            settle_ms,
        } = resize(json!({"width": 800, "height": 600.5})).unwrap()
        else {
            panic!("not a resize")
        };
        assert_eq!((width, height, settle_ms), (800.0, 600.5, 2000));
        assert!(resize(json!({"width": 64, "height": 8192})).is_ok());
        for (w, h) in [(63.0, 600.0), (800.0, 8193.0), (0.0, 0.0), (-800.0, 600.0)] {
            let error = resize(json!({"width": w, "height": h})).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{w}x{h}");
        }
        let infinite = ResizeArgs {
            width: f32::INFINITY,
            height: f32::NAN,
            settle_ms: 0,
        };
        assert_eq!(
            infinite.request().unwrap_err().code(),
            ErrorCode::InvalidArgument
        );
        assert!(args::<ResizeArgs>(json!({"width": 800})).is_err());
    }

    #[test]
    fn a_drop_needs_absolute_paths_and_drops_unless_cancelled() {
        let drop = |value: Value| args::<DropArgs>(value).unwrap().request();
        let Request::Drop {
            target,
            paths,
            cancel,
            timeout_ms,
            settle_ms,
        } = drop(json!({"target": {"id": ["files"]}, "paths": ["/tmp/a", "/does/not/exist"]}))
            .unwrap()
        else {
            panic!("not a drop")
        };
        assert_eq!(
            target,
            Target::Id {
                id: vec![Segment::Str("files".into())]
            }
        );
        assert_eq!(
            paths,
            [PathBuf::from("/tmp/a"), PathBuf::from("/does/not/exist")]
        );
        assert!(!cancel);
        assert_eq!((timeout_ms, settle_ms), (5000, 2000));
        let cancelled = drop(json!({"target": {"x": 1, "y": 2}, "paths": ["/a"], "cancel": true}));
        assert!(matches!(cancelled, Ok(Request::Drop { cancel: true, .. })));
        for bad in [json!([]), json!(["relative/file"]), json!(["/a", "b"])] {
            let error = drop(json!({"target": {"x": 1, "y": 2}, "paths": bad})).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{bad}");
        }
        assert!(args::<DropArgs>(json!({"target": {"x": 1, "y": 2}})).is_err());
    }

    #[test]
    fn scroll_modifiers_and_window_close_map_to_protocol_five() {
        let scroll: ScrollArgs =
            args(json!({"target": {"x": 5, "y": 6}, "dy": 120, "modifiers": ["ctrl", "cmd"]}))
                .unwrap();
        let Request::Scroll { modifiers, .. } = scroll.request().unwrap() else {
            panic!("not a scroll")
        };
        assert_eq!(modifiers, [Modifier::Ctrl, Modifier::Super]);
        assert!(
            args::<ScrollArgs>(
                json!({"target": {"x": 5, "y": 6}, "dy": 1, "modifiers": ["hyper"]})
            )
            .is_err()
        );
        let kill: CloseArgs = args(json!({})).unwrap();
        assert_eq!((kill.mode, kill.keep_data), (CloseMode::Kill, false));
        let window: CloseArgs = args(json!({"mode": "window", "keep_data": true})).unwrap();
        assert_eq!(window.mode, CloseMode::Window);
        assert_eq!(
            window.window_request(),
            Request::CloseWindow {
                timeout_ms: 2000,
                settle_ms: 2000
            }
        );
        assert!(args::<CloseArgs>(json!({"mode": "nicely"})).is_err());
    }

    #[test]
    fn targets_take_exactly_one_shape_and_unknown_fields_fail() {
        let role: TargetArg = args(json!({"role": "button", "name": "Count", "nth": 1})).unwrap();
        assert_eq!(
            Target::from(role),
            Target::Role {
                role: "button".into(),
                name: "Count".into(),
                nth: Some(1)
            }
        );
        assert!(args::<TargetArg>(json!({"panel": 3})).is_err());
        assert!(args::<TargetArg>(json!({"id": ["a"], "x": 1, "y": 2})).is_err());
        let ints: TargetArg = args(json!({"id": ["list", 3]})).unwrap();
        assert_eq!(
            Target::from(ints),
            Target::Id {
                id: vec![Segment::Str("list".into()), Segment::Int(3)]
            }
        );
        assert!(args::<ClickArgs>(json!({"target": {"id": ["a"]}, "buton": "left"})).is_err());
        assert!(args::<LaunchArgs>(json!({"mode": "remote"})).is_err());
    }

    #[test]
    fn a_bad_target_names_the_accepted_shapes() {
        for bad in [
            json!({"point": [1, 2]}),
            json!({"x": 1}),
            json!({"id": "count"}),
            json!({"id": [true]}),
            json!({"id": ["a"], "x": 1, "y": 2}),
            json!([1, 2]),
        ] {
            for error in [
                args::<ClickArgs>(json!({"target": bad.clone()}))
                    .unwrap_err()
                    .to_string(),
                args::<WaitArgs>(json!({"target": bad.clone()}))
                    .unwrap_err()
                    .to_string(),
            ] {
                for shape in [
                    r#"{"id": [segment, ...]}"#,
                    r#"{"id_suffix": "text"}"#,
                    r#"{"x": number, "y": number}"#,
                ] {
                    assert!(error.contains(shape), "{bad}: {error}");
                }
                assert!(!error.contains("untagged"), "{error}");
            }
        }
        let suffix: ClickArgs = args(json!({"target": {"id_suffix": "json.yml"}})).unwrap();
        assert!(
            matches!(suffix.request(), Request::Click { target: Target::IdSuffix { ref id_suffix }, .. } if id_suffix == "json.yml")
        );
    }

    #[test]
    fn the_target_schema_still_lists_every_shape() {
        let schema = serde_json::to_value(schemars::schema_for!(TargetArg)).unwrap();
        let shapes = schema["anyOf"]
            .as_array()
            .expect("an untagged enum is anyOf");
        assert_eq!(shapes.len(), 4);
        let required: Vec<_> = shapes.iter().map(|s| s["required"].clone()).collect();
        assert!(required.contains(&json!(["id_suffix"])), "{required:?}");
    }

    #[test]
    fn pick_file_takes_paths_or_cancel_but_not_both() {
        let pick: PickFileArgs = args(json!({"paths": ["notes.txt", "/abs/b.txt"]})).unwrap();
        assert_eq!(
            pick.request().unwrap(),
            Request::PickFile {
                answer: PickAnswer::Paths(vec!["notes.txt".into(), "/abs/b.txt".into()]),
                settle_ms: 2000,
                timeout_ms: Some(10_000),
            }
        );
        let cancel: PickFileArgs = args(json!({"cancel": true})).unwrap();
        assert_eq!(
            cancel.request().unwrap(),
            Request::PickFile {
                answer: PickAnswer::Cancel,
                settle_ms: 2000,
                timeout_ms: Some(10_000),
            }
        );
        for bad in [
            json!({}),
            json!({"paths": []}),
            json!({"paths": ["a"], "cancel": true}),
        ] {
            let pick: PickFileArgs = args(bad.clone()).unwrap();
            assert!(pick.request().is_err(), "{bad}");
        }
    }

    #[test]
    fn structured_results_drop_the_tag_and_absent_fields() {
        let reply = Reply::PickFile {
            answered: Answered::Queued,
            prompt: None,
            settled: true,
        };
        assert_eq!(
            structured(&reply),
            json!({"answered": "queued", "settled": true})
        );
        let node = Node::new(
            vec![Segment::Str("count".into())],
            Rect::new(1.0, 2.0, 3.0, 4.0),
            true,
            0,
        );
        let click = structured(&Reply::Click {
            clicked: node.clone(),
            hit: node,
            settled: false,
        });
        assert_eq!(click["clicked"]["path"], json!(["count"]));
        assert_eq!(click["hit"]["path"], json!(["count"]));
    }

    #[test]
    fn wait_takes_a_target_or_a_duration_and_hover_a_target() {
        let late = || Target::Id {
            id: vec![Segment::Str("late".into())],
        };
        let visible: WaitArgs = args(json!({"target": {"id": ["late"]}})).unwrap();
        assert_eq!(
            visible.request().unwrap(),
            Request::Wait {
                until: WaitUntil::Visible(late()),
                timeout_ms: 5000
            }
        );
        let gone: WaitArgs =
            args(json!({"target": {"id": ["late"]}, "gone": true, "timeout_ms": 50})).unwrap();
        assert_eq!(
            gone.request().unwrap(),
            Request::Wait {
                until: WaitUntil::Gone(late()),
                timeout_ms: 50
            }
        );
        let slept: WaitArgs = args(json!({"ms": 250})).unwrap();
        assert_eq!(
            slept.request().unwrap(),
            Request::Wait {
                until: WaitUntil::Elapsed(250),
                timeout_ms: 250
            }
        );
        for bad in [
            json!({}),
            json!({"ms": 30_001}),
            json!({"ms": 5, "target": {"id": ["a"]}}),
            json!({"target": {"x": 1, "y": 2}}),
            json!({"ms": 5, "gone": true}),
        ] {
            let wait: WaitArgs = args(bad.clone()).unwrap();
            assert_eq!(
                wait.request().unwrap_err().code(),
                ErrorCode::InvalidArgument,
                "{bad}"
            );
        }
        let hover: HoverArgs = args(json!({"target": {"id": ["count"]}})).unwrap();
        assert_eq!(
            hover.request(),
            Request::Hover {
                target: Target::Id {
                    id: vec![Segment::Str("count".into())]
                },
                timeout_ms: 5000,
                settle_ms: 2000
            }
        );
    }

    #[test]
    fn drag_steps_default_to_ten_and_stay_within_one_to_a_hundred() {
        let parse = |value: Value| serde_json::from_value::<DragArgs>(value).unwrap();
        let ends = json!({"from": {"id": ["chip"]}, "to": {"x": 1.0, "y": 2.0}});
        let Request::Drag {
            steps,
            timeout_ms,
            settle_ms,
            ..
        } = parse(ends.clone()).request().unwrap()
        else {
            panic!("a drag request");
        };
        assert_eq!((steps, timeout_ms, settle_ms), (10, 5000, 2000));
        for bad in [0, 101] {
            let mut args = ends.clone();
            args["steps"] = json!(bad);
            let error = parse(args).request().unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{bad}");
        }
        assert!(serde_json::from_value::<DialogArgs>(json!({"label": "x"})).is_err());
    }

    #[test]
    fn scroll_takes_pixels_or_notches_but_not_both() {
        let scroll = |value: Value| args::<ScrollArgs>(value).unwrap().request();
        let at = || json!({"x": 5, "y": 6});
        let Request::Scroll { wheel, dx, dy, .. } =
            scroll(json!({"target": at(), "notches": 3})).unwrap()
        else {
            panic!("a scroll")
        };
        assert_eq!((wheel, dx, dy), (Some(Wheel::new(0, 3, 16)), 0.0, 0.0));
        let Request::Scroll { wheel, .. } =
            scroll(json!({"target": at(), "notches_x": -2, "notch_ms": 5})).unwrap()
        else {
            panic!("a scroll")
        };
        assert_eq!(wheel, Some(Wheel::new(-2, 0, 5)));
        let Request::Scroll { wheel, dx, dy, .. } =
            scroll(json!({"target": at(), "dx": 30})).unwrap()
        else {
            panic!("a scroll")
        };
        assert_eq!((wheel, dx, dy), (None, 30.0, 0.0));
        for bad in [
            json!({"target": at()}),
            json!({"target": at(), "notches": 2, "dy": 40}),
            json!({"target": at(), "notches_x": 2, "dx": 1}),
            json!({"target": at(), "notches": 201}),
            json!({"target": at(), "notches_x": -201}),
            json!({"target": at(), "notches": 0}),
            json!({"target": at(), "notches": 1, "notch_ms": 0}),
            json!({"target": at(), "notches": 1, "notch_ms": 1001}),
            json!({"target": at(), "dy": 40, "notch_ms": 16}),
        ] {
            let error = scroll(bad.clone()).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{bad}");
        }
    }

    #[test]
    fn a_notch_scroll_budgets_its_notches() {
        let parse = |value: Value| args::<ScrollArgs>(value).unwrap();
        let at = || json!({"x": 1, "y": 2});
        assert_eq!(parse(json!({"target": at(), "dy": 4})).notch_time_ms(), 0);
        let both = json!({"target": at(), "notches": -3, "notches_x": 2, "notch_ms": 10});
        assert_eq!(parse(both).notch_time_ms(), 50);
        assert_eq!(
            parse(json!({"target": at(), "notches": 4})).notch_time_ms(),
            64
        );
    }

    #[test]
    fn frames_starts_with_an_idle_threshold_and_stops_bare() {
        let frames = |value: Value| args::<FramesArgs>(value);
        let request = |value: Value| frames(value).unwrap().request();
        assert_eq!(
            request(json!({"action": "start"})).unwrap(),
            Request::FramesStart { idle_ms: 100 }
        );
        assert_eq!(
            request(json!({"action": "start", "idle_ms": 0})).unwrap(),
            Request::FramesStart { idle_ms: 0 }
        );
        assert_eq!(
            request(json!({"action": "stop"})).unwrap(),
            Request::FramesStop
        );
        for bad in [
            json!({"action": "start", "idle_ms": 10_001}),
            json!({"action": "stop", "idle_ms": 5}),
        ] {
            assert_eq!(
                request(bad.clone()).unwrap_err().code(),
                ErrorCode::InvalidArgument,
                "{bad}"
            );
        }
        assert!(frames(json!({"action": "pause"})).is_err());
        assert!(frames(json!({})).is_err());
    }
}
