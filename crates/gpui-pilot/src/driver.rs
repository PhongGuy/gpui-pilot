//! The `Driver`: one app at a time through launch, requests, screenshots, crashes and close.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use gpui_pilot_protocol::{
    Answered, DEFAULT_REFRESH_HZ, DRAG_BUTTON_VERSION, DROP_VERSION, ErrorCode, LaunchParams,
    MODIFIERS_VERSION, MouseButton, Outcome, PROTOCOL_VERSION, PickAnswer, PilotError,
    REFRESH_RATES_HZ, RESIZE_VERSION, Reply, Request, Target,
};
use gpui_pilot_session::SessionError;
use gpui_pilot_session::sandbox;
use serde_json::{Value, json};

use crate::backend::BackendError;
use crate::config::{Config, Mode, arg_binds};
use crate::error::ToolError;
use crate::launch::{
    Host, LaunchDir, Timeouts, app_env, exe_was_replaced, log_tail, private_file, run_build,
    session_command, spawn_host, start_host,
};
use crate::params::{
    CloseArgs, LaunchArgs, Network, ScreenshotArgs, TypeArgs, absolute, structured,
};
use crate::redact::{scrub_error, typed_result};
use crate::stage::Stage;

/// Added to a request's own budget before the server gives up on its reply.
pub(crate) const REPLY_MARGIN: Duration = Duration::from_secs(30);
/// The longest launch settle; with the host's startup it stays inside the hello timeout.
const MAX_LAUNCH_SETTLE_MS: u64 = 20_000;
/// How long a host whose socket closed gets to report its exit status.
const CRASH_WAIT: Duration = Duration::from_secs(2);
/// Characters a headless `type` sends per request. Every key can cost the app a frame, so each
/// chunk gets its own reply deadline and the app draws between chunks.
const TYPE_CHUNK_CHARS: usize = 40;
/// A screenshot's own budget; `REPLY_MARGIN` comes on top.
const SCREENSHOT_BUDGET: Duration = Duration::from_secs(2);
/// Added to the hello timeout in session mode, so a slow KWin (5 s), window (15 s) or tree
/// (15 s) stage reports its own name rather than `stage hello`.
const SESSION_HELLO_EXTRA: Duration = Duration::from_secs(35);
/// How long `close` waits for the `Closed` reply.
const CLOSE_LIMIT: Duration = Duration::from_secs(5);
/// How long `close` waits for the exit before the group is killed.
const EXIT_WAIT: Duration = Duration::from_secs(10);

pub(crate) struct Driver {
    root: PathBuf,
    temp: PathBuf,
    timeouts: Timeouts,
    /// The PATH the session preflight searches for its host tools.
    tool_path: OsString,
    running: Option<Running>,
    /// The closed app's launch dir, parked by `close` with `keep_data` for the next launch.
    kept: Option<Kept>,
    /// Every text `type` redacted (or sent in a call that failed), scrubbed from later replies
    /// until the next `launch`, since an app may echo it (a plain input's value in `tree`).
    secrets: Vec<String>,
}

/// Fields drop in declaration order: the host's process group is killed and reaped before
/// its launch directory is removed.
struct Running {
    host: Host,
    dir: LaunchDir,
    shots: u32,
    /// Maps session pick paths; `None` for a headless app, which answers with host paths.
    stage: Option<Stage>,
    mode: Mode,
    /// The folder this app was configured, built and launched from.
    project: PathBuf,
}

/// A closed app's launch dir (and pick dirs), parked for a `keep_data` launch. Removed when
/// replaced, when a launch without `keep_data` starts, and when the driver drops.
struct Kept {
    dir: LaunchDir,
    mode: Mode,
    stage: Option<Stage>,
    /// The kept app's project folder; data is reused only by a launch from the same one.
    project: PathBuf,
}

/// The request a pick sends, and the paths the app is given. Headless (`stage` None): the
/// timeout is removed, so a protocol 2 host sees its old frame, and no paths are reported.
/// Session: `Paths` mapped through `stage.map`; `Cancel` and the timeout pass unchanged.
pub(crate) fn pick_request(
    request: Request,
    stage: Option<&mut Stage>,
) -> Result<(Request, Option<Vec<PathBuf>>), ToolError> {
    let Request::PickFile {
        answer,
        settle_ms,
        timeout_ms,
    } = request
    else {
        return Ok((request, None));
    };
    let Some(stage) = stage else {
        let sent = Request::PickFile {
            answer,
            settle_ms,
            timeout_ms: None,
        };
        return Ok((sent, None));
    };
    let (answer, paths) = match answer {
        PickAnswer::Paths(paths) => {
            let mapped = stage.map(&paths)?;
            (PickAnswer::Paths(mapped.clone()), Some(mapped))
        }
        other => (other, None),
    };
    let sent = Request::PickFile {
        answer,
        settle_ms,
        timeout_ms,
    };
    Ok((sent, paths))
}

/// The launch's refresh rate: the launch argument, then the mode's config, then the default.
fn refresh_hz(launch: Option<u32>, config: Option<u32>) -> u32 {
    launch.or(config).unwrap_or(DEFAULT_REFRESH_HZ)
}

/// Refuses a session output whose device width libxcvt would silently change at its rate,
/// naming the nearest logical widths it keeps.
fn check_output(output: &sandbox::Output) -> Result<(), ToolError> {
    let Err(refused) = output.check_custom_mode() else {
        return Ok(());
    };
    let width = |w: Option<u32>| w.map_or_else(|| "none".to_owned(), |w| w.to_string());
    Err(ToolError::new(
        ErrorCode::InvalidArgument,
        format!(
            "at {} Hz libxcvt would turn the device width {} into {}; the nearest logical widths \
             it keeps at scale {} are {} below and {} above, and `refresh_hz: 60` accepts any width",
            output.refresh_hz,
            refused.device_width,
            refused.cvt_width,
            output.scale,
            width(refused.below),
            width(refused.above),
        ),
    )
    .with("field", "width"))
}

/// Fails the launch when the session output advertises a rate other than the derived one,
/// which is how KWin reports a custom mode it fell back from.
fn check_refresh(expected_mhz: u32, advertised_mhz: u32) -> Result<(), ToolError> {
    if expected_mhz == advertised_mhz {
        return Ok(());
    }
    Err(ToolError::new(
        ErrorCode::LaunchFailed,
        format!(
            "the session output runs at {advertised_mhz} mHz, not the derived {expected_mhz} mHz"
        ),
    ))
}

/// The sandbox's view of a session network; `host` never reaches a sandbox.
fn sandbox_network(network: Network) -> sandbox::Network {
    match network {
        Network::Pasta => sandbox::Network::Pasta,
        _ => sandbox::Network::None,
    }
}

/// Checks the host tools a session on `network` needs, searching `tool_path`.
fn preflight(tool_path: &OsStr, network: Network) -> Result<(), ToolError> {
    sandbox::preflight_in(tool_path, sandbox_network(network)).map_err(|error| match error {
        SessionError::EnvironmentMissing { ref tool, .. } => {
            let tool = tool.clone();
            ToolError::new(ErrorCode::EnvironmentMissing, error.to_string()).with("tool", tool)
        }
        other => ToolError::from(PilotError::from(other)),
    })
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Headless => "headless",
        Mode::Session => "session",
    }
}

fn not_running() -> ToolError {
    ToolError::new(
        ErrorCode::NotRunning,
        "no app is running; call launch first",
    )
}

/// Refuses, before anything reaches the host, a request that uses a field the host's protocol
/// predates: `modifiers` on `click` and `drag` need a protocol 8 host, and a non-left `button`
/// on `drag` a protocol 9 one, `drop` a protocol 10 one and `resize` a protocol 11 one; an
/// older host would close the connection on them or refuse them as unreadable.
fn refuse_unsupported(request: &Request, host_version: u32) -> Result<(), ToolError> {
    if matches!(request, Request::Resize { .. }) && host_version < RESIZE_VERSION {
        return Err(ToolError::new(
            ErrorCode::Unsupported,
            format!(
                "resize needs a host built from kit-0.7.1-p18 or later; \
                 this host speaks protocol {host_version}"
            ),
        ));
    }
    if matches!(request, Request::Drop { .. }) && host_version < DROP_VERSION {
        return Err(ToolError::new(
            ErrorCode::Unsupported,
            format!(
                "drop needs a host built from kit-0.7.1-p17 or later; \
                 this host speaks protocol {host_version}"
            ),
        ));
    }
    if let Request::Drag { button, .. } = request
        && *button != MouseButton::Left
        && host_version < DRAG_BUTTON_VERSION
    {
        return Err(ToolError::new(
            ErrorCode::Unsupported,
            format!(
                "`button` on drag needs a host built from kit-0.7.1-p15 or later; \
                 this host speaks protocol {host_version}"
            ),
        ));
    }
    let (Request::Click { modifiers, .. } | Request::Drag { modifiers, .. }) = request else {
        return Ok(());
    };
    if modifiers.is_empty() || host_version >= MODIFIERS_VERSION {
        return Ok(());
    }
    Err(ToolError::new(
        ErrorCode::Unsupported,
        format!(
            "modifiers on click and drag need a host built from kit-0.7.1-p12 or later; \
             this host speaks protocol {host_version}"
        ),
    ))
}

/// A backend call's outcome as a reply or a tool error; a closed connection is the caller's
/// to interpret.
fn reply_of(outcome: Result<Outcome, BackendError>, limit: Duration) -> Result<Reply, ToolError> {
    match outcome {
        Ok(Outcome::Ok(reply)) => Ok(reply),
        Ok(Outcome::Err(error)) => Err(error.into()),
        Ok(other) => Err(ToolError::new(
            ErrorCode::Internal,
            format!("unexpected outcome {other:?}"),
        )),
        Err(BackendError::Closed) => Err(ToolError::new(
            ErrorCode::AppCrashed,
            "the app closed its connection",
        )),
        Err(BackendError::Timeout) => Err(ToolError::new(
            ErrorCode::Timeout,
            format!("no reply from the app within {limit:?}; it is still running"),
        )),
        Err(BackendError::Malformed(why)) => Err(ToolError::new(
            ErrorCode::Internal,
            format!("malformed reply from the app: {why}"),
        )),
    }
}

/// `error` with the count of characters known typed, in its message and a `typed` field. The
/// count is a lower bound: the chunk that failed may be partly typed.
fn typed_progress(mut error: ToolError, typed: usize, total: usize) -> ToolError {
    error.error.message = format!(
        "{} (at least {typed} of {total} characters were typed; the chunk that failed may be partly typed)",
        error.error.message
    );
    error.with("typed", typed)
}

/// One piece of a chunked `type`: its text and the timeout and settle it carries.
#[derive(Debug, PartialEq)]
struct TypePiece {
    text: String,
    timeout_ms: u64,
    settle_ms: u64,
}

/// Splits `text` into `TYPE_CHUNK_CHARS` pieces on `char` boundaries, which is how the host
/// types (one keystroke per `char`), so no piece separates anything the host keeps together.
/// The focusing timeout rides the first piece and the settle only the last.
fn type_pieces(text: &str, timeout_ms: u64, settle_ms: u64) -> Vec<TypePiece> {
    let chars: Vec<char> = text.chars().collect();
    let count = chars.chunks(TYPE_CHUNK_CHARS).count();
    chars
        .chunks(TYPE_CHUNK_CHARS)
        .enumerate()
        .map(|(i, piece)| TypePiece {
            text: piece.iter().collect(),
            timeout_ms: if i == 0 { timeout_ms } else { 0 },
            settle_ms: if i + 1 == count { settle_ms } else { 0 },
        })
        .collect()
}

/// Polls `child` until it exits or `limit` passes; `None` when it is still running.
fn wait_for_exit(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => return None,
        }
    }
}

impl Driver {
    /// `root` holds `gpui-pilot.toml`; launch directories go under `temp`.
    pub fn new(root: PathBuf, temp: PathBuf, timeouts: Timeouts) -> Driver {
        Driver {
            root,
            temp,
            timeouts,
            tool_path: std::env::var_os("PATH").unwrap_or_default(),
            running: None,
            kept: None,
            secrets: Vec::new(),
        }
    }

    /// The PATH `preflight` searches. `Driver::new` sets `tool_path` from the server's own
    /// `PATH`; only tests replace it.
    #[cfg(test)]
    pub fn with_tool_path(self, path: OsString) -> Driver {
        Driver {
            tool_path: path,
            ..self
        }
    }

    /// The folder relative tool paths resolve against: the running app's project folder, else
    /// the server root.
    pub fn base(&self) -> &Path {
        self.running.as_ref().map_or(&self.root, |r| &r.project)
    }

    /// `{ mode, network, pid, window: { w, h, scale }, data_dir, reused, build_ms, start_ms,
    /// refresh_hz, settled }`; `refresh_hz` is the rate the display really runs at.
    pub fn launch(&mut self, args: LaunchArgs) -> Result<Value, ToolError> {
        // Bad arguments fail before anything touches the running app or the config.
        if args.settle_ms > MAX_LAUNCH_SETTLE_MS {
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                format!("`settle_ms` is at most {MAX_LAUNCH_SETTLE_MS}"),
            ));
        }
        // The backends derive periods and CVT modes from the rate, so only measured rates pass.
        if let Some(hz) = args.refresh_hz
            && !REFRESH_RATES_HZ.contains(&hz)
        {
            return Err(ToolError::new(
                ErrorCode::InvalidArgument,
                format!("`refresh_hz` must be one of {REFRESH_RATES_HZ:?}, not {hz}"),
            )
            .with("field", "refresh_hz"));
        }
        let env = app_env(&args.env)?;
        // The folder that supplies config, build, binary and binds for this launch. Its path is
        // re-read at use; whoever could swap a parent folder in between could edit the config.
        let project = match &args.project_dir {
            Some(dir) => crate::project::resolve(&self.root, dir)?,
            None => self.root.clone(),
        };
        // A host that exited on its own is forgotten, so it never reports AlreadyRunning.
        if let Some(running) = &mut self.running
            && !matches!(running.host.group.child().try_wait(), Ok(None))
        {
            self.running = None;
        }
        // Kept data is checked before anything else can fail, and a launch that does not ask
        // for it drops what was kept.
        if args.keep_data {
            self.check_keep_data(&args, &project)?;
        } else {
            self.kept = None;
        }
        let unsupported = |message: &str| {
            ToolError::new(ErrorCode::Unsupported, message)
                .with("mode", args.mode)
                .with("tool", "launch")
        };
        // Headless shares the host's network and cannot leave it; session always gets a
        // private one, cut off unless pasta is asked for.
        let network = match (args.mode, args.network) {
            (Mode::Headless, None | Some(Network::Host)) => Network::Host,
            (Mode::Headless, Some(_)) => {
                return Err(unsupported(
                    "headless mode runs on the host network; omit `network` or pass `host`",
                ));
            }
            (Mode::Session, Some(Network::Host)) => {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgument,
                    "session mode never runs on the host network; pass `none` or `pasta`",
                ));
            }
            (Mode::Session, chosen) => chosen.unwrap_or(Network::None),
        };
        // Host ports only make sense through pasta; the X11 TCP ports would reach the desktop.
        let bad_ports = |why: &str| ToolError::new(ErrorCode::InvalidArgument, why);
        let never_pasta =
            args.mode == Mode::Headless || args.network.is_some_and(|n| n != Network::Pasta);
        if !args.host_ports.is_empty() && never_pasta {
            return Err(bad_ports(
                "`host_ports` needs session mode with `network: pasta`",
            ));
        }
        if args
            .host_ports
            .iter()
            .any(|&port| port == 0 || (6000..=6063).contains(&port))
        {
            return Err(bad_ports(
                "`host_ports` refuses port 0 and the X11 ports 6000-6063",
            ));
        }
        if args.mode == Mode::Session {
            preflight(&self.tool_path, network)?;
        }
        // Checked before `replace` closes anything, so a refused path never costs the running
        // app; it needs no config. Session args naming host paths are bound read-write.
        let extra_binds = match args.mode {
            Mode::Session => arg_binds(&args.args).map_err(|why| {
                ToolError::new(ErrorCode::InvalidArgument, why).with("field", "args")
            })?,
            _ => Vec::new(),
        };
        if let Some(running) = &self.running {
            if !args.replace {
                return Err(ToolError::new(
                    ErrorCode::AlreadyRunning,
                    "an app is already running; close it or pass replace: true",
                )
                .with("pid", running.host.pid));
            }
            // close drops the old app whatever its outcome, which is all replace needs.
            let _ = self.close(args.keep_data);
        }
        let config = Config::load(&project)?;
        let section = config.section(args.mode)?;
        // A session launch that names no network takes the config's default, checked like an
        // explicit one.
        let explicit = network;
        let network = match (args.mode, args.network) {
            (Mode::Session, None) => section.network.unwrap_or(Network::None),
            _ => network,
        };
        if !args.host_ports.is_empty() && network != Network::Pasta {
            return Err(bad_ports(
                "`host_ports` needs session mode with `network: pasta`",
            ));
        }
        let mut bound = section.clone();
        bound.binds.extend(extra_binds);
        let section = &bound;
        if network != explicit {
            preflight(&self.tool_path, network)?;
        }
        let sandboxed = sandbox_network(network);
        let rate = refresh_hz(args.refresh_hz, section.refresh_hz);
        // Rounded as the agent rounds, so the virtual output matches the requested size.
        let output = sandbox::Output::new(
            args.width.round() as u32,
            args.height.round() as u32,
            args.scale,
        )
        .with_refresh_hz(rate);
        if args.mode == Mode::Session {
            check_output(&output)?;
        }
        let build = if args.build {
            run_build(&section.build, &project)?
        } else {
            Duration::ZERO
        };
        // Session dirs always sit under /tmp so the private runtime path stays short enough for
        // KIO's socket names, whatever TMPDIR is.
        let temp = match args.mode {
            Mode::Session => Path::new("/tmp"),
            _ => self.temp.as_path(),
        };
        // A kept dir is only taken now that the config and build are fine; every failure
        // below parks it again.
        let kept = if args.keep_data {
            self.kept.take()
        } else {
            None
        };
        let (dir, kept_stage) = match kept {
            Some(Kept { dir, stage, .. }) => match dir.reuse() {
                Ok(()) => (dir, stage),
                Err(e) => {
                    let mode = args.mode;
                    self.kept = Some(Kept {
                        dir,
                        mode,
                        stage,
                        project,
                    });
                    return Err(ToolError::new(
                        ErrorCode::LaunchFailed,
                        format!("cannot reuse the kept data: {e}"),
                    ));
                }
            },
            None => {
                let dir = LaunchDir::create(temp).map_err(|e| {
                    ToolError::new(
                        ErrorCode::LaunchFailed,
                        format!("cannot create a launch directory: {e}"),
                    )
                })?;
                (dir, None)
            }
        };
        let reused = args.keep_data;
        let start = Instant::now();
        let started = (|| {
            let mut params =
                LaunchParams::new(args.width, args.height, args.scale, dir.data(), args.args);
            params.settle_ms = args.settle_ms;
            params.refresh_hz = rate;
            params.reused = reused;
            if args.mode == Mode::Session {
                let failed = |what: &str, e: std::io::Error| {
                    ToolError::new(ErrorCode::LaunchFailed, format!("{what}: {e}"))
                };
                let exe =
                    std::env::current_exe().map_err(|e| failed("cannot find the server", e))?;
                // A `cargo install` over a running server leaves a dead path that bwrap cannot bind.
                if exe_was_replaced(&exe) {
                    return Err(ToolError::new(
                        ErrorCode::LaunchFailed,
                        "the gpui-pilot binary was replaced on disk since this server started; \
                         restart the MCP server",
                    ));
                }
                let command = session_command(
                    section,
                    &project,
                    &dir,
                    &exe,
                    output,
                    sandboxed,
                    &args.host_ports,
                    &env,
                )
                .map_err(|e| failed("cannot prepare the sandbox", e))?;
                let timeouts = Timeouts {
                    hello: self.timeouts.hello + SESSION_HELLO_EXTRA,
                    ..self.timeouts
                };
                let (host, window) = start_host(command, &dir, params, PROTOCOL_VERSION, timeouts)?;
                // Dropping the host kills its group, so the sandbox is gone before the error.
                if let Err(error) = check_refresh(output.expected_refresh_mhz(), host.refresh_mhz) {
                    drop(host);
                    return Err(error.with("log_tail", log_tail(&dir)));
                }
                Ok((host, window))
            } else {
                spawn_host(section, &project, &dir, params, &env, self.timeouts)
            }
        })();
        let (host, window) = match started {
            Ok(started) => started,
            Err(error) => {
                if reused {
                    let (mode, stage) = (args.mode, kept_stage);
                    self.kept = Some(Kept {
                        dir,
                        mode,
                        stage,
                        project,
                    });
                }
                return Err(error);
            }
        };
        let start_ms = start.elapsed().as_millis() as u64;
        // A relaunch may name different paths, so the stage is always rebuilt from this
        // launch's binds, numbering picks on from the kept one.
        let stage = (args.mode == Mode::Session).then(|| {
            let stage = Stage::new(&dir, &section.binds);
            match &kept_stage {
                Some(kept) => stage.continue_from(kept),
                None => stage,
            }
        });
        let result = json!({
            "mode": args.mode,
            "network": network,
            "pid": host.pid,
            "window": window,
            "data_dir": dir.data(),
            "reused": reused,
            "build_ms": build.as_millis() as u64,
            "start_ms": start_ms,
            "refresh_hz": host.refresh_mhz as f64 / 1000.0,
            "settled": host.settled,
        });
        // Only a started host ends the old app, so only then do its secrets stop mattering.
        self.secrets.clear();
        self.running = Some(Running {
            host,
            dir,
            shots: 0,
            stage,
            mode: args.mode,
            project,
        });
        Ok(result)
    }

    /// The `keep_data` rules: a replaced app must be in the requested mode, and with nothing
    /// running a kept dir of that mode must exist. Either way the data must come from the
    /// launch's `project`, so one branch's data never meets another branch's build.
    fn check_keep_data(&self, args: &LaunchArgs, project: &Path) -> Result<(), ToolError> {
        let invalid = |message: String| ToolError::new(ErrorCode::InvalidArgument, message);
        let from = match (&self.running, &self.kept) {
            (Some(running), _) if args.replace => Some(&running.project),
            (None, Some(kept)) => Some(&kept.project),
            _ => None,
        };
        if let Some(from) = from
            && from != project
        {
            return Err(invalid(format!(
                "the kept data is from the project at {}; launch from it to reuse the data",
                from.display()
            ))
            .with("field", "project_dir"));
        }
        match (&self.running, &self.kept) {
            (Some(running), _) if args.replace && running.mode != args.mode => {
                Err(invalid(format!(
                    "the running app is in {} mode; its data cannot start a {} app",
                    mode_name(running.mode),
                    mode_name(args.mode)
                )))
            }
            (None, None) => Err(invalid(
                "no kept data; close with keep_data first".to_owned(),
            )),
            (None, Some(kept)) if kept.mode != args.mode => Err(invalid(format!(
                "the kept data is from a {} app; launch in {} mode",
                mode_name(kept.mode),
                mode_name(kept.mode)
            ))),
            _ => Ok(()),
        }
    }

    /// Forwards `request`, waiting up to `budget + REPLY_MARGIN`.
    pub fn call(&mut self, request: Request, budget: Duration) -> Result<Reply, ToolError> {
        let running = self.running.as_mut().ok_or_else(not_running)?;
        refuse_unsupported(&request, running.host.version)?;
        let limit = budget + REPLY_MARGIN;
        match running.host.backend.call(request, limit) {
            Err(BackendError::Closed) => Err(self.crashed()),
            outcome => reply_of(outcome, limit),
        }
    }

    /// Types `args.text`; with `args.redact`, or when the host reports the focused input
    /// masked, the result and any error carry `<redacted N chars>` instead of the text.
    pub fn type_text(&mut self, args: TypeArgs) -> Result<Value, ToolError> {
        let budget = Duration::from_millis(args.timeout_ms.saturating_add(args.settle_ms));
        let (text, redact) = (args.text.clone(), args.redact);
        // Remembered before sending: an error reply cannot say whether the input was masked,
        // so the text stays a secret unless the host answers that it was not.
        let known = self.secrets.contains(&text);
        self.remember(&text);
        match self.type_chunks(args, budget) {
            Ok(reply) => {
                let result = typed_result(&reply, &text, redact);
                if result.get("redacted").is_none() && !known {
                    self.secrets.retain(|s| *s != text);
                }
                Ok(result)
            }
            Err(error) if redact => Err(scrub_error(error, &text)),
            Err(error) => Err(error),
        }
    }

    /// Sends a long headless `type` as `TYPE_CHUNK_CHARS` pieces, each with its own reply
    /// deadline. The focusing click rides the first piece and only the last one settles, so a
    /// text of one chunk or less is a single plain call. A failure reports a lower bound on
    /// the characters typed, since the failing chunk may be partly typed.
    fn type_chunks(&mut self, args: TypeArgs, budget: Duration) -> Result<Reply, ToolError> {
        let headless = self
            .running
            .as_ref()
            .is_some_and(|r| r.mode == Mode::Headless);
        if !headless || args.text.chars().count() <= TYPE_CHUNK_CHARS {
            return self.call(args.request(), budget);
        }
        let total = args.text.chars().count();
        let mut target = args.target.map(Into::into);
        let mut typed = 0;
        let mut masked = false;
        let mut settled = false;
        for piece in type_pieces(&args.text, args.timeout_ms, args.settle_ms) {
            let budget = Duration::from_millis(piece.timeout_ms.saturating_add(piece.settle_ms));
            let length = piece.text.chars().count();
            let request = Request::Type {
                text: piece.text,
                target: target.take(),
                timeout_ms: piece.timeout_ms,
                settle_ms: piece.settle_ms,
            };
            match self.call(request, budget) {
                Ok(Reply::Type {
                    settled: done,
                    masked: hidden,
                }) => {
                    masked |= hidden;
                    settled = done;
                    typed += length;
                }
                Ok(other) => {
                    return Err(ToolError::new(
                        ErrorCode::Internal,
                        format!("unexpected reply {other:?}"),
                    ));
                }
                Err(error) => return Err(typed_progress(error, typed, total)),
            }
        }
        Ok(Reply::Type { settled, masked })
    }

    /// The texts `type` has redacted so far, for scrubbing from every reply.
    pub fn secrets(&self) -> &[String] {
        &self.secrets
    }

    fn remember(&mut self, text: &str) {
        if !text.is_empty() && !self.secrets.iter().any(|s| s == text) {
            self.secrets.push(text.to_owned());
        }
    }

    /// `{ closed: false, settled }`, or after a closed window (or a lost connection during the
    /// call) the `close` teardown's result with `closed: true`.
    pub fn close_window(&mut self, args: CloseArgs) -> Result<Value, ToolError> {
        let running = self.running.as_mut().ok_or_else(not_running)?;
        let budget = Duration::from_millis(args.timeout_ms.saturating_add(args.settle_ms));
        let limit = budget + REPLY_MARGIN;
        // A backend that closed its window ends itself, so a lost connection here is the
        // close succeeding, not a crash.
        match running.host.backend.call(args.window_request(), limit) {
            Err(BackendError::Closed) => {}
            outcome => match reply_of(outcome, limit)? {
                Reply::CloseWindow { closed: true, .. } => {}
                reply @ Reply::CloseWindow { closed: false, .. } => return Ok(structured(&reply)),
                other => {
                    return Err(ToolError::new(
                        ErrorCode::Internal,
                        format!("unexpected reply {other:?}"),
                    ));
                }
            },
        }
        let mut result = self.close(args.keep_data)?;
        result["closed"] = json!(true);
        Ok(result)
    }

    /// Forwards a `PickFile` built by `PickFileArgs::request`, budgeting `settle_ms` plus, in
    /// session mode, `timeout_ms`. Relative answer paths resolve against `base`. The result is
    /// the reply's fields, plus `paths` when an open or save answer carried mapped paths.
    pub fn pick_file(&mut self, request: Request) -> Result<Value, ToolError> {
        let request = match request {
            Request::PickFile {
                answer: PickAnswer::Paths(paths),
                settle_ms,
                timeout_ms,
            } => Request::PickFile {
                answer: PickAnswer::Paths(paths.iter().map(|p| absolute(self.base(), p)).collect()),
                settle_ms,
                timeout_ms,
            },
            other => other,
        };
        let running = self.running.as_mut().ok_or_else(not_running)?;
        // Paths are staged before the call, which needs the driver back.
        let (request, paths) = pick_request(request, running.stage.as_mut())?;
        let budget = match &request {
            Request::PickFile {
                settle_ms,
                timeout_ms,
                ..
            } => settle_ms.saturating_add(timeout_ms.unwrap_or(0)),
            _ => 0,
        };
        let reply = self.call(request, Duration::from_millis(budget))?;
        let mut result = structured(&reply);
        if let (
            Reply::PickFile {
                answered: Answered::Open | Answered::Save,
                ..
            },
            Some(paths),
        ) = (&reply, paths)
        {
            result["paths"] = json!(paths);
        }
        Ok(result)
    }

    /// Forgets the app whose socket closed, reporting its log and, once it exits, its code.
    fn crashed(&mut self) -> ToolError {
        let Some(mut running) = self.running.take() else {
            return not_running();
        };
        let status = wait_for_exit(running.host.group.child(), CRASH_WAIT);
        let mut error = ToolError::new(ErrorCode::AppCrashed, "the app closed its connection")
            .with("log_tail", log_tail(&running.dir));
        if let Some(code) = status.and_then(|s| s.code()) {
            error = error.with("exit_code", code);
        }
        error
    }

    /// The structured result (`w`, `h`, `scale`, and `saved_to` when asked) and the PNG bytes.
    pub fn screenshot(&mut self, args: ScreenshotArgs) -> Result<(Value, Vec<u8>), ToolError> {
        let running = self.running.as_mut().ok_or_else(not_running)?;
        let internal = |message: String| ToolError::new(ErrorCode::Internal, message);
        // The host always writes into the launch directory, the one place a sandboxed agent
        // can reach; a pre-created private file keeps the shot at mode 0600 there.
        running.shots += 1;
        let path = running
            .dir
            .path()
            .join(format!("shot-{}.png", running.shots));
        private_file(&path)
            .map_err(|e| internal(format!("cannot create {}: {e}", path.display())))?;
        let request = Request::Screenshot {
            target: args.target.map(Target::from),
            path: path.clone(),
        };
        let reply = self.call(request, SCREENSHOT_BUDGET)?;
        let Reply::Screenshot { .. } = reply else {
            return Err(internal(format!("unexpected reply {reply:?}")));
        };
        let mut result = structured(&reply);
        let png = std::fs::read(&path)
            .map_err(|e| internal(format!("cannot read {}: {e}", path.display())))?;
        if let Some(save_to) = &args.save_to {
            let save_to = absolute(self.base(), save_to);
            std::fs::write(&save_to, &png)
                .map_err(|e| internal(format!("writing {}: {e}", save_to.display())))?;
            result["saved_to"] = json!(save_to);
        }
        Ok((result, png))
    }

    /// `{ exit_code?, log_tail, data_dir? }`; `data_dir` only when `keep_data` parked the dir.
    pub fn close(&mut self, keep_data: bool) -> Result<Value, ToolError> {
        let mut running = self.running.take().ok_or_else(not_running)?;
        // The reply does not matter: end of stream also stops the host.
        let _ = running.host.backend.call(Request::Close, CLOSE_LIMIT);
        running.host.backend.shutdown();
        let status = wait_for_exit(running.host.group.child(), EXIT_WAIT);
        let mut result = json!({ "log_tail": log_tail(&running.dir) });
        // Dropping the host kills whatever is left of the group before the dir can go.
        let Running {
            host,
            dir,
            stage,
            mode,
            project,
            ..
        } = running;
        drop(host);
        if keep_data {
            result["data_dir"] = json!(dir.data());
            self.kept = Some(Kept {
                dir,
                mode,
                stage,
                project,
            });
        }
        if let Some(code) = status.and_then(|s| s.code()) {
            result["exit_code"] = json!(code);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::config::CONFIG_FILE;
    use crate::launch::ProcessGroup;
    use crate::launch::tests::short_temp;
    use gpui_pilot_protocol::{Modifier, PickAnswer, Request};
    use serde_json::json;
    use std::os::unix::process::CommandExt;

    #[test]
    fn a_failed_chunk_reports_a_lower_bound_on_the_characters_typed() {
        let error = typed_progress(ToolError::new(ErrorCode::Timeout, "no reply"), 80, 300);
        assert_eq!(error.details["typed"], json!(80));
        assert!(
            error
                .error
                .message
                .contains("at least 80 of 300 characters were typed; the chunk that failed may be partly typed"),
            "{}",
            error.error.message
        );
    }

    #[test]
    fn forty_characters_are_one_piece_with_click_and_settle_together() {
        let pieces = type_pieces(&"a".repeat(40), 500, 200);
        assert_eq!(pieces.len(), 1);
        assert_eq!((pieces[0].timeout_ms, pieces[0].settle_ms), (500, 200));
    }

    #[test]
    fn forty_one_characters_split_forty_and_one() {
        let pieces = type_pieces(&"a".repeat(41), 500, 200);
        let lens: Vec<usize> = pieces.iter().map(|p| p.text.chars().count()).collect();
        assert_eq!(lens, [40, 1]);
        assert_eq!((pieces[0].timeout_ms, pieces[0].settle_ms), (500, 0));
        assert_eq!((pieces[1].timeout_ms, pieces[1].settle_ms), (0, 200));
    }

    #[test]
    fn multibyte_text_splits_on_char_boundaries_only() {
        // Two bytes, three bytes, four bytes, and a base letter plus a combining accent; the
        // host types one keystroke per char, so char boundaries are the right cut.
        let text = "é漢😀e\u{301}".repeat(23);
        let pieces = type_pieces(&text, 0, 0);
        assert!(
            pieces
                .iter()
                .all(|p| p.text.chars().count() <= TYPE_CHUNK_CHARS)
        );
        let joined: String = pieces.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(joined, text);
        assert_eq!(pieces[0].text.chars().count(), 40);
    }

    fn fixture(config: Option<&str>) -> (Driver, tempfile::TempDir) {
        let root = short_temp();
        if let Some(text) = config {
            std::fs::write(root.path().join(CONFIG_FILE), text).unwrap();
        }
        let quick = Timeouts {
            socket: Duration::from_millis(300),
            hello: Duration::from_secs(1),
        };
        let driver = Driver::new(root.path().to_path_buf(), root.path().to_path_buf(), quick);
        (driver, root)
    }

    fn launch(value: Value) -> LaunchArgs {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn an_unmeasured_refresh_rate_fails_before_the_config() {
        let (mut driver, _root) = fixture(None);
        let code = |driver: &mut Driver, hz: u32| {
            let args = launch(json!({"mode": "headless", "refresh_hz": hz}));
            driver.launch(args).unwrap_err()
        };
        for hz in [60, 120, 144, 240] {
            assert_eq!(
                code(&mut driver, hz).code(),
                ErrorCode::InvalidConfig,
                "{hz} passes; the config is missing"
            );
        }
        for hz in [0, 30, 75, 165] {
            let error = code(&mut driver, hz);
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{hz}");
            assert_eq!(error.details["field"], json!("refresh_hz"));
        }
    }

    #[test]
    fn the_launch_rate_beats_the_mode_config_which_beats_144() {
        assert_eq!(refresh_hz(Some(60), Some(240)), 60);
        assert_eq!(refresh_hz(None, Some(240)), 240);
        assert_eq!(refresh_hz(None, None), 144);
    }

    #[test]
    fn requests_before_launch_are_not_running() {
        let (mut driver, _root) = fixture(None);
        let key = Request::Key {
            keys: "escape".into(),
            settle_ms: 0,
        };
        assert_eq!(
            driver.call(key, Duration::ZERO).unwrap_err().code(),
            ErrorCode::NotRunning
        );
        assert_eq!(
            driver.close(false).unwrap_err().code(),
            ErrorCode::NotRunning
        );
        let shot = ScreenshotArgs {
            target: None,
            save_to: None,
        };
        assert_eq!(
            driver.screenshot(shot).unwrap_err().code(),
            ErrorCode::NotRunning
        );
    }

    #[test]
    fn a_headless_pick_never_carries_the_timeout() {
        let request = Request::PickFile {
            answer: PickAnswer::Cancel,
            settle_ms: 5,
            timeout_ms: Some(10_000),
        };
        let (sent, paths) = pick_request(request, None).unwrap();
        assert_eq!(
            sent,
            Request::PickFile {
                answer: PickAnswer::Cancel,
                settle_ms: 5,
                timeout_ms: None
            }
        );
        assert_eq!(paths, None);
    }

    #[test]
    fn a_session_pick_maps_its_paths_and_keeps_the_timeout() {
        let parent = crate::launch::tests::short_temp();
        let dir = crate::launch::LaunchDir::create(parent.path()).unwrap();
        std::fs::create_dir_all(dir.pick_in()).unwrap();
        std::fs::create_dir_all(dir.pick_out()).unwrap();
        let mut stage = Stage::new(&dir, &[]);
        let outside = crate::launch::tests::short_temp();
        let file = outside.path().join("in.txt");
        std::fs::write(&file, "x").unwrap();
        let request = Request::PickFile {
            answer: PickAnswer::Paths(vec![file]),
            settle_ms: 5,
            timeout_ms: Some(700),
        };
        let (sent, paths) = pick_request(request, Some(&mut stage)).unwrap();
        let staged = vec![dir.pick_in().join("1/in.txt")];
        assert_eq!(
            sent,
            Request::PickFile {
                answer: PickAnswer::Paths(staged.clone()),
                settle_ms: 5,
                timeout_ms: Some(700)
            }
        );
        assert_eq!(paths, Some(staged));
        let cancel = Request::PickFile {
            answer: PickAnswer::Cancel,
            settle_ms: 5,
            timeout_ms: Some(700),
        };
        assert_eq!(
            pick_request(cancel.clone(), Some(&mut stage)).unwrap(),
            (cancel, None)
        );
    }

    #[test]
    fn pick_file_before_launch_is_not_running() {
        let (mut driver, _root) = fixture(None);
        let request = Request::PickFile {
            answer: PickAnswer::Cancel,
            settle_ms: 0,
            timeout_ms: Some(10),
        };
        assert_eq!(
            driver.pick_file(request).unwrap_err().code(),
            ErrorCode::NotRunning
        );
    }

    #[test]
    fn session_without_the_tools_is_environment_missing_and_headless_pasta_unsupported() {
        let (driver, _root) = fixture(None);
        let mut driver = driver.with_tool_path("/nonexistent".into());
        let error = driver
            .launch(launch(json!({"mode": "session"})))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::EnvironmentMissing);
        assert_eq!(error.details["tool"], json!("bwrap"));
        let error = driver
            .launch(launch(json!({"mode": "headless", "network": "pasta"})))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unsupported);
    }

    #[test]
    fn host_ports_need_pasta_and_refuse_zero_and_x11() {
        let (driver, _root) = fixture(None);
        let mut driver = driver.with_tool_path("/nonexistent".into());
        for args in [
            json!({"mode": "session", "network": "none", "host_ports": [8080]}),
            json!({"mode": "headless", "host_ports": [8080]}),
            json!({"mode": "session", "network": "pasta", "host_ports": [0]}),
            json!({"mode": "session", "network": "pasta", "host_ports": [6000]}),
            json!({"mode": "session", "network": "pasta", "host_ports": [6063]}),
        ] {
            let error = driver.launch(launch(args.clone())).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{args}");
        }
    }

    #[test]
    fn headless_runs_on_the_host_network_and_session_never_does() {
        let (mut driver, _root) = fixture(None);
        for network in ["none", "pasta"] {
            let error = driver
                .launch(launch(json!({"mode": "headless", "network": network})))
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unsupported, "{network}");
            assert!(
                error.error.message.contains("host network"),
                "{}",
                error.error.message
            );
        }
        // `host` passes the network check; the missing config is what fails.
        let host = driver.launch(launch(json!({"mode": "headless", "network": "host"})));
        assert_eq!(host.unwrap_err().code(), ErrorCode::InvalidConfig);
        let session = driver.launch(launch(json!({"mode": "session", "network": "host"})));
        assert_eq!(session.unwrap_err().code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn launch_settle_is_capped_at_twenty_seconds() {
        let (mut driver, _root) = fixture(None);
        let code = |driver: &mut Driver, settle_ms: u64| {
            let args = launch(json!({"mode": "headless", "settle_ms": settle_ms}));
            driver.launch(args).unwrap_err().code()
        };
        // At the cap the check passes and the missing config is what fails.
        assert_eq!(code(&mut driver, 20_000), ErrorCode::InvalidConfig);
        assert_eq!(code(&mut driver, 20_001), ErrorCode::InvalidArgument);
    }

    #[test]
    fn a_width_libxcvt_would_change_is_refused_with_the_nearest_ones() {
        let error =
            check_output(&sandbox::Output::new(1100, 600, 1.0).with_refresh_hz(144)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(error.details["field"], json!("width"));
        let message = &error.error.message;
        assert!(
            message.contains("1096") && message.contains("1104"),
            "{message}"
        );
        assert!(message.contains("refresh_hz"), "{message}");
        assert!(
            check_output(&sandbox::Output::new(1100, 600, 1.0)).is_ok(),
            "60 Hz takes any width"
        );
        assert!(check_output(&sandbox::Output::new(1280, 800, 1.5).with_refresh_hz(144)).is_ok());
    }

    #[test]
    fn an_advertised_rate_other_than_the_derived_one_fails_the_launch() {
        assert!(check_refresh(143_832, 143_832).is_ok());
        let error = check_refresh(143_832, 60_000).unwrap_err();
        assert_eq!(error.code(), ErrorCode::LaunchFailed);
        let message = &error.error.message;
        assert!(
            message.contains("143832") && message.contains("60000"),
            "{message}"
        );
    }

    #[test]
    fn a_type_text_is_remembered_before_sending_and_kept_across_a_failed_launch() {
        let (mut driver, _root) = fixture(None);
        let args: TypeArgs = serde_json::from_value(json!({"text": "hunter2"})).unwrap();
        // Not running: the call fails, and masking is unknown, so the text stays remembered.
        assert_eq!(
            driver.type_text(args).unwrap_err().code(),
            ErrorCode::NotRunning
        );
        assert_eq!(driver.secrets(), ["hunter2".to_owned()]);
        // Bad arguments and a missing config both fail before a host starts.
        let bad = driver
            .launch(launch(json!({"mode": "headless", "settle_ms": 999_999})))
            .unwrap_err();
        assert_eq!(bad.code(), ErrorCode::InvalidArgument);
        let unconfigured = driver
            .launch(launch(json!({"mode": "headless"})))
            .unwrap_err();
        assert_eq!(unconfigured.code(), ErrorCode::InvalidConfig);
        assert_eq!(driver.secrets(), ["hunter2".to_owned()]);
    }

    #[test]
    fn launch_reports_config_build_and_start_failures() {
        let (mut driver, _root) = fixture(None);
        assert_eq!(
            driver
                .launch(launch(json!({"mode": "headless"})))
                .unwrap_err()
                .code(),
            ErrorCode::InvalidConfig
        );
        let (mut driver, _root) = fixture(Some(
            "[headless]\nbuild = [\"sh\", \"-c\", \"echo broken; exit 1\"]\nbinary = \"/usr/bin/true\"\n",
        ));
        let error = driver
            .launch(launch(json!({"mode": "headless"})))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::BuildFailed);
        assert_eq!(error.details["log_tail"], json!("broken"));
        let error = driver
            .launch(launch(json!({"mode": "headless", "build": false})))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::LaunchFailed);
        assert_eq!(
            driver.close(false).unwrap_err().code(),
            ErrorCode::NotRunning
        );
    }

    #[test]
    fn a_long_temp_root_fails_the_launch_at_the_socket_limit() {
        let (_, root) = fixture(Some("[headless]\nbinary = \"/usr/bin/true\"\n"));
        let long = root.path().join("t".repeat(100));
        std::fs::create_dir(&long).unwrap();
        let mut driver = Driver::new(root.path().to_path_buf(), long, Timeouts::default());
        let error = driver
            .launch(launch(json!({"mode": "headless", "build": false})))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::LaunchFailed);
        assert!(
            error.error.message.contains("107"),
            "{}",
            error.error.message
        );
    }

    /// Stores `program` as the running host in `mode`, as a launch would; returns its launch dir.
    fn hold(driver: &mut Driver, program: &str, args: &[&str], mode: Mode) -> PathBuf {
        let project = driver.root.clone();
        hold_in(driver, program, args, mode, project, |_| {})
    }

    /// `hold` launched from `project`, with `peer` running on the host's end of the socket.
    fn hold_in(
        driver: &mut Driver,
        program: &str,
        args: &[&str],
        mode: Mode,
        project: PathBuf,
        peer: impl FnOnce(std::os::unix::net::UnixStream) + Send + 'static,
    ) -> PathBuf {
        let child = std::process::Command::new(program)
            .args(args)
            .process_group(0)
            .spawn()
            .unwrap();
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        std::thread::spawn(move || peer(theirs));
        let host = Host {
            pid: child.id(),
            group: ProcessGroup::new(child),
            backend: Backend::new(ours).unwrap(),
            settled: true,
            refresh_mhz: 60_000,
            version: PROTOCOL_VERSION,
        };
        let dir = LaunchDir::create(&driver.temp).unwrap();
        let path = dir.path().to_path_buf();
        driver.running = Some(Running {
            host,
            dir,
            shots: 0,
            stage: None,
            mode,
            project,
        });
        path
    }

    #[test]
    fn a_dead_host_is_forgotten_and_a_live_one_is_already_running() {
        let (mut driver, _root) = fixture(None);
        hold(&mut driver, "/usr/bin/sleep", &["60"], Mode::Headless);
        let busy = driver
            .launch(launch(json!({"mode": "headless"})))
            .unwrap_err();
        assert_eq!(busy.code(), ErrorCode::AlreadyRunning);
        hold(&mut driver, "/usr/bin/true", &[], Mode::Headless);
        std::thread::sleep(Duration::from_millis(200));
        let fresh = driver
            .launch(launch(json!({"mode": "headless"})))
            .unwrap_err();
        assert_eq!(
            fresh.code(),
            ErrorCode::InvalidConfig,
            "the dead host was forgotten"
        );
        assert!(driver.running.is_none());
    }

    #[test]
    fn close_with_keep_data_keeps_the_dir_until_a_fresh_launch() {
        let (mut driver, _root) = fixture(None);
        let path = hold(&mut driver, "/usr/bin/true", &[], Mode::Headless);
        let closed = driver.close(true).unwrap();
        assert_eq!(closed["data_dir"], json!(path.join("data")));
        assert!(path.join("home").is_dir() && path.join("data").is_dir());
        let fresh = driver
            .launch(launch(json!({"mode": "headless"})))
            .unwrap_err();
        assert_eq!(fresh.code(), ErrorCode::InvalidConfig);
        assert!(
            !path.exists(),
            "a launch without keep_data removed the kept dir"
        );
    }

    #[test]
    fn close_without_keep_data_removes_the_dir_and_reports_no_data_dir() {
        let (mut driver, _root) = fixture(None);
        let path = hold(&mut driver, "/usr/bin/true", &[], Mode::Headless);
        let closed = driver.close(false).unwrap();
        assert!(closed.get("data_dir").is_none(), "{closed}");
        assert!(!path.exists());
    }

    #[test]
    fn keep_data_needs_kept_data_of_the_same_mode_and_survives_a_failed_launch() {
        let (mut driver, _root) = fixture(None);
        let nothing = driver
            .launch(launch(json!({"mode": "headless", "keep_data": true})))
            .unwrap_err();
        assert_eq!(nothing.code(), ErrorCode::InvalidArgument);
        let path = hold(&mut driver, "/usr/bin/true", &[], Mode::Headless);
        driver.close(true).unwrap();
        let other = driver
            .launch(launch(json!({"mode": "session", "keep_data": true})))
            .unwrap_err();
        assert_eq!(other.code(), ErrorCode::InvalidArgument);
        assert!(
            other.error.message.contains("headless"),
            "{}",
            other.error.message
        );
        assert!(
            path.is_dir(),
            "a refused launch leaves the kept dir in place"
        );
        let same = driver
            .launch(launch(json!({"mode": "headless", "keep_data": true})))
            .unwrap_err();
        assert_eq!(same.code(), ErrorCode::InvalidConfig);
        assert!(
            path.is_dir(),
            "a launch that fails before starting keeps the kept dir"
        );
    }

    #[test]
    fn replace_with_keep_data_keeps_the_running_apps_dir() {
        let (mut driver, _root) = fixture(None);
        let path = hold(&mut driver, "/usr/bin/sleep", &["2"], Mode::Headless);
        let mismatch = driver
            .launch(launch(
                json!({"mode": "session", "keep_data": true, "replace": true}),
            ))
            .unwrap_err();
        assert_eq!(mismatch.code(), ErrorCode::InvalidArgument);
        assert!(
            driver.running.is_some(),
            "a refused launch leaves the app running"
        );
        let relaunch = driver
            .launch(launch(
                json!({"mode": "headless", "keep_data": true, "replace": true}),
            ))
            .unwrap_err();
        assert_eq!(relaunch.code(), ErrorCode::InvalidConfig);
        assert!(driver.running.is_none());
        assert!(
            path.join("data").is_dir(),
            "replace kept the closed app's dir"
        );
    }

    #[test]
    fn modifiers_are_refused_for_a_host_older_than_protocol_eight_and_nothing_else_is() {
        let target = Target::Point { x: 1.0, y: 2.0 };
        let click = |modifiers| Request::Click {
            target: target.clone(),
            button: MouseButton::Left,
            count: 1,
            modifiers,
            timeout_ms: 0,
            settle_ms: 0,
        };
        let drag = |modifiers| Request::Drag {
            from: target.clone(),
            to: target.clone(),
            steps: 1,
            button: MouseButton::Left,
            modifiers,
            timeout_ms: 0,
            settle_ms: 0,
        };
        for request in [click(vec![Modifier::Ctrl]), drag(vec![Modifier::Shift])] {
            let error = refuse_unsupported(&request, MODIFIERS_VERSION - 1).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unsupported);
            assert!(
                error.error.message.contains("kit-0.7.1-p12")
                    && error.error.message.contains("protocol 7"),
                "{}",
                error.error.message
            );
            assert!(refuse_unsupported(&request, MODIFIERS_VERSION).is_ok());
        }
        // Without modifiers an old host is served exactly as before.
        for request in [click(Vec::new()), drag(Vec::new())] {
            assert!(refuse_unsupported(&request, MODIFIERS_VERSION - 1).is_ok());
        }
    }

    #[test]
    fn a_right_or_middle_drag_is_refused_for_a_host_before_protocol_nine_and_a_left_one_is_not() {
        let target = Target::Point { x: 1.0, y: 2.0 };
        let drag = |button| Request::Drag {
            from: target.clone(),
            to: target.clone(),
            steps: 1,
            button,
            modifiers: Vec::new(),
            timeout_ms: 0,
            settle_ms: 0,
        };
        for (button, old) in [(MouseButton::Right, 7), (MouseButton::Middle, 8)] {
            let error = refuse_unsupported(&drag(button), old).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unsupported);
            assert!(
                error.error.message.contains("`button` on drag")
                    && error.error.message.contains("kit-0.7.1-p15")
                    && error.error.message.contains(&format!("protocol {old}")),
                "{}",
                error.error.message
            );
            assert!(refuse_unsupported(&drag(button), DRAG_BUTTON_VERSION).is_ok());
        }
        assert!(refuse_unsupported(&drag(MouseButton::Left), 7).is_ok());
    }

    #[test]
    fn a_drop_is_refused_for_a_host_before_protocol_ten() {
        let drop = Request::Drop {
            target: Target::Point { x: 1.0, y: 2.0 },
            paths: vec!["/tmp/a".into()],
            cancel: false,
            timeout_ms: 0,
            settle_ms: 0,
        };
        for old in [7, 8, DROP_VERSION - 1] {
            let error = refuse_unsupported(&drop, old).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unsupported);
            assert!(
                error.error.message.contains("drop")
                    && error.error.message.contains("kit-0.7.1-p17")
                    && error.error.message.contains(&format!("protocol {old}")),
                "{}",
                error.error.message
            );
        }
        assert!(refuse_unsupported(&drop, DROP_VERSION).is_ok());
    }

    #[test]
    fn a_resize_is_refused_for_a_host_before_protocol_eleven() {
        let resize = Request::Resize {
            width: 800.0,
            height: 600.0,
            settle_ms: 0,
        };
        for old in [7, 9, RESIZE_VERSION - 1] {
            let error = refuse_unsupported(&resize, old).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unsupported);
            assert!(
                error.error.message.contains("resize")
                    && error.error.message.contains("kit-0.7.1-p18")
                    && error.error.message.contains(&format!("protocol {old}")),
                "{}",
                error.error.message
            );
        }
        assert!(refuse_unsupported(&resize, RESIZE_VERSION).is_ok());
    }

    /// A driver rooted at a repository with a worktree; each folder gets its config text.
    /// Returns the driver, the temp dir holding both, the root and the worktree.
    fn worktree_fixture(
        root_config: &str,
        tree_config: &str,
    ) -> (Driver, tempfile::TempDir, PathBuf, PathBuf) {
        let temp = short_temp();
        let base = temp.path().canonicalize().unwrap();
        let (root, tree) = (base.join("root"), base.join("wt"));
        crate::project::tests::repo(&root);
        crate::project::tests::worktree(&root, &tree, "wt");
        std::fs::write(root.join(CONFIG_FILE), root_config).unwrap();
        std::fs::write(tree.join(CONFIG_FILE), tree_config).unwrap();
        let quick = Timeouts {
            socket: Duration::from_millis(300),
            hello: Duration::from_secs(1),
        };
        let driver = Driver::new(root.clone(), base, quick);
        (driver, temp, root, tree)
    }

    fn in_project(dir: &Path, extra: Value) -> LaunchArgs {
        let mut value = json!({"mode": "headless", "project_dir": dir});
        let fields = extra.as_object().unwrap().clone();
        value.as_object_mut().unwrap().extend(fields);
        launch(value)
    }

    #[test]
    fn a_bad_project_dir_fails_before_the_running_app_is_closed() {
        let (mut driver, temp, _root, _tree) = worktree_fixture("", "");
        hold(&mut driver, "/usr/bin/sleep", &["60"], Mode::Headless);
        // An absolute folder holding a config but no worktree reaches the git checks.
        let plain = temp.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join(CONFIG_FILE), "").unwrap();
        for dir in [json!("wt"), json!(plain)] {
            let error = driver
                .launch(launch(
                    json!({"mode": "headless", "replace": true, "project_dir": dir}),
                ))
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{dir}");
            assert_eq!(error.details["field"], json!("project_dir"));
            assert!(driver.running.is_some(), "the running app was left alone");
        }
    }

    #[test]
    fn the_config_and_build_come_from_the_project_dir() {
        let (mut driver, _temp, root, tree) = worktree_fixture(
            "[headless]\nbuild = [\"sh\", \"-c\", \"echo root; exit 1\"]\nbinary = \"/usr/bin/true\"\n",
            "[headless]\nbuild = [\"sh\", \"-c\", \"touch built; echo worktree; exit 1\"]\nbinary = \"/usr/bin/true\"\n",
        );
        let error = driver.launch(in_project(&tree, json!({}))).unwrap_err();
        assert_eq!(error.code(), ErrorCode::BuildFailed);
        assert_eq!(error.details["log_tail"], json!("worktree"));
        assert!(
            tree.join("built").is_file(),
            "the build ran in the worktree"
        );
        assert!(!root.join("built").exists());
    }

    #[test]
    fn the_binary_is_resolved_against_the_project_dir() {
        use std::os::unix::fs::PermissionsExt;
        let (mut driver, _temp, root, tree) =
            worktree_fixture("", "[headless]\nbinary = \"app.sh\"\n");
        for dir in [&root, &tree] {
            let app = dir.join("app.sh");
            std::fs::write(&app, "#!/bin/sh\n/usr/bin/touch \"$0.ran\"\n").unwrap();
            std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let error = driver
            .launch(in_project(&tree, json!({"build": false})))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::LaunchFailed);
        assert!(
            tree.join("app.sh.ran").is_file(),
            "the worktree's binary ran"
        );
        assert!(!root.join("app.sh.ran").exists());
    }

    #[test]
    fn keep_data_from_another_project_dir_is_refused_and_keeps_the_data() {
        let (mut driver, _temp, root, tree) = worktree_fixture("", "");
        let path = hold(&mut driver, "/usr/bin/true", &[], Mode::Headless);
        driver.close(true).unwrap();
        let other = driver
            .launch(in_project(&tree, json!({"keep_data": true})))
            .unwrap_err();
        assert_eq!(other.code(), ErrorCode::InvalidArgument);
        assert!(
            other.error.message.contains(root.to_str().unwrap()),
            "{}",
            other.error.message
        );
        assert!(path.join("data").is_dir(), "the kept data is intact");
        let same = driver
            .launch(launch(json!({"mode": "headless", "keep_data": true})))
            .unwrap_err();
        assert_eq!(
            same.code(),
            ErrorCode::InvalidConfig,
            "the root's data fits"
        );
        assert!(path.join("data").is_dir());
        // With replace, the running app's project dir is the one compared.
        hold_in(
            &mut driver,
            "/usr/bin/sleep",
            &["60"],
            Mode::Headless,
            tree,
            |_| {},
        );
        let replaced = driver
            .launch(launch(
                json!({"mode": "headless", "keep_data": true, "replace": true}),
            ))
            .unwrap_err();
        assert_eq!(replaced.code(), ErrorCode::InvalidArgument);
        assert!(
            driver.running.is_some(),
            "a refused launch leaves the app running"
        );
    }

    /// A fake host answering picks and screenshots (writing a stand-in PNG), reporting each
    /// request on `seen`; it stops at anything else.
    fn answering_host(
        seen: std::sync::mpsc::Sender<Request>,
    ) -> impl FnOnce(std::os::unix::net::UnixStream) + Send + 'static {
        use gpui_pilot_protocol::{BackendFrame, ClientFrame, read_line, write_line};
        move |stream| {
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            while let Ok(Some(frame)) = read_line::<ClientFrame>(&mut reader) {
                let reply = match frame.request.clone() {
                    Request::PickFile { .. } => Reply::PickFile {
                        answered: Answered::Open,
                        prompt: None,
                        settled: true,
                    },
                    Request::Screenshot { path, .. } => {
                        std::fs::write(path, b"png").unwrap();
                        Reply::Screenshot {
                            w: 1,
                            h: 1,
                            scale: 1.0,
                        }
                    }
                    _ => return,
                };
                seen.send(frame.request).unwrap();
                let outcome = Outcome::Ok(reply);
                write_line(&mut writer, &BackendFrame::new(frame.id, outcome)).unwrap();
            }
        }
    }

    #[test]
    fn relative_pick_and_save_paths_resolve_against_the_running_project_dir() {
        let (mut driver, _temp, root, tree) = worktree_fixture("", "");
        let (seen, requests) = std::sync::mpsc::channel();
        let host = answering_host(seen);
        let project = tree.clone();
        hold_in(
            &mut driver,
            "/usr/bin/sleep",
            &["60"],
            Mode::Headless,
            project,
            host,
        );
        assert_eq!(driver.base(), tree);
        let pick = Request::PickFile {
            answer: PickAnswer::Paths(vec!["in.txt".into(), "/abs/b.txt".into()]),
            settle_ms: 0,
            timeout_ms: Some(0),
        };
        driver.pick_file(pick).unwrap();
        let Request::PickFile {
            answer: PickAnswer::Paths(paths),
            ..
        } = requests.recv().unwrap()
        else {
            panic!("not a pick");
        };
        assert_eq!(paths, [tree.join("in.txt"), PathBuf::from("/abs/b.txt")]);
        let shot = ScreenshotArgs {
            target: None,
            save_to: Some("shot.png".into()),
        };
        let (result, _) = driver.screenshot(shot).unwrap();
        assert_eq!(result["saved_to"], json!(tree.join("shot.png")));
        assert!(tree.join("shot.png").is_file());
        driver.close(false).unwrap();
        assert_eq!(driver.base(), root, "a closed app no longer sets the base");
    }

    #[test]
    fn a_crashed_app_no_longer_sets_the_base() {
        let (mut driver, _temp, root, tree) = worktree_fixture("", "");
        hold_in(
            &mut driver,
            "/usr/bin/true",
            &[],
            Mode::Headless,
            tree,
            |_| {},
        );
        let key = Request::Key {
            keys: "escape".into(),
            settle_ms: 0,
        };
        let crashed = driver.call(key, Duration::ZERO).unwrap_err();
        assert_eq!(crashed.code(), ErrorCode::AppCrashed);
        assert_eq!(driver.base(), root);
    }
}
