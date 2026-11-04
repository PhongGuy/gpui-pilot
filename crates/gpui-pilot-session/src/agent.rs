//! The session agent: a protocol 4 backend that runs the app inside the private session.

use std::ffi::OsString;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::BufReader;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gpui_pilot_protocol::{
    BackendFrame, ClientFrame, ErrorCode, LaunchParams, MAX_NOTCHES, MouseButton, NOTCH_MS_RANGE,
    Node, Outcome, PROTOCOL_VERSION, PilotError, REFRESH_RATES_HZ, Rect, Reply, Request, Target,
    WaitUntil, WindowInfo, hover_wait_ms, read_line, read_line_lenient, write_line,
};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::process::{
    Pid, PidfdFlags, Signal, WaitOptions, pidfd_open, pidfd_send_signal, waitpid,
};

use crate::SessionError;
use crate::resolve::{Actionable, actionable, at_point, resolve};
use crate::sandbox::Output;
use crate::session::Session;

/// How long the app gets to map its window, and then to publish a stable tree.
const BRING_UP: Duration = Duration::from_secs(15);
/// The poll interval for the tree and for settle captures.
const TICK: Duration = Duration::from_millis(50);
/// How long captures must stay unchanged before the app counts as settled, as the host's
/// `QUIET_LAYOUT_MS`: an app can paint a hover frame and its content up to 130 ms apart.
const QUIET: Duration = Duration::from_millis(160);
/// How long `pick_file` waits for a dialog when the request names no timeout.
const PICK_TIMEOUT_MS: u64 = 10_000;

/// Settle's rule over timestamped capture hashes: settled once the latest hash has held for
/// `quiet` since the first capture in its unbroken run.
struct Quiet {
    quiet: Duration,
    since: Option<(Instant, u64)>,
}

impl Quiet {
    fn new(quiet: Duration) -> Self {
        Self { quiet, since: None }
    }

    /// Records the capture taken at `at`; true when `hash` equals the current run's and the run
    /// began at least `quiet` before `at`. A different hash starts a new run.
    fn observe(&mut self, at: Instant, hash: u64) -> bool {
        match self.since {
            Some((start, held)) if held == hash => at.duration_since(start) >= self.quiet,
            _ => {
                self.since = Some((at, hash));
                false
            }
        }
    }
}

/// What `gpui-pilot session-agent` was asked to run: extra environment, then the app.
#[non_exhaustive]
#[derive(Debug, PartialEq)]
pub struct AgentArgs {
    pub env: Vec<(String, String)>,
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

/// `[--env NAME=VALUE]... -- PROGRAM [ARG]...`; anything else is an error message.
pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<AgentArgs, String> {
    let mut args = args.into_iter();
    let mut env = Vec::new();
    // Options end at `--`; everything after it belongs to the app, untouched.
    loop {
        let Some(arg) = args.next() else {
            return Err("expected `-- PROGRAM [ARG]...`".into());
        };
        match arg.to_str() {
            Some("--") => break,
            Some("--env") => {
                let pair = args.next().ok_or("--env needs NAME=VALUE")?;
                let pair = pair
                    .into_string()
                    .map_err(|pair| format!("--env {pair:?} is not UTF-8"))?;
                match pair.split_once('=') {
                    Some((name, value)) if !name.is_empty() => {
                        env.push((name.to_owned(), value.to_owned()))
                    }
                    _ => return Err(format!("--env {pair:?} is not NAME=VALUE")),
                }
            }
            _ => return Err(format!("unexpected argument {arg:?} before `--`")),
        }
    }
    let program = args.next().ok_or("expected a PROGRAM after `--`")?;
    Ok(AgentArgs {
        env,
        program: program.into(),
        args: args.collect(),
    })
}

/// Runs inside the sandbox. Exit 0 after `Close` or end of stream, 1 when the hello fails or
/// the app exits, 2 on bad arguments or a missing `GPUI_PILOT_SOCKET`.
pub fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("gpui-pilot session-agent: {message}");
            return ExitCode::from(2);
        }
    };
    let Some(socket) = std::env::var_os("GPUI_PILOT_SOCKET") else {
        eprintln!("gpui-pilot session-agent: GPUI_PILOT_SOCKET is not set");
        return ExitCode::from(2);
    };
    let (mut reader, mut writer) = match UnixStream::connect(&socket)
        .and_then(|stream| Ok((BufReader::new(stream.try_clone()?), stream)))
    {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("gpui-pilot session-agent: connecting to {socket:?}: {error}");
            return ExitCode::from(1);
        }
    };
    let (id, launch) = match handshake(&mut reader) {
        Ok(hello) => hello,
        Err((id, error)) => {
            answer(&mut writer, id, Outcome::Err(error));
            return ExitCode::from(1);
        }
    };
    match Agent::bring_up(parsed, &launch) {
        Ok((agent, hello)) => {
            if !answer(&mut writer, id, Outcome::Ok(hello)) {
                return ExitCode::from(1);
            }
            agent.serve(reader, writer)
        }
        Err(error) => {
            eprintln!("gpui-pilot session-agent: {error}");
            answer(&mut writer, id, Outcome::Err(error.into()));
            ExitCode::from(1)
        }
    }
}

/// Reads the first frame, which must be `Hello` at this protocol version; a failure carries
/// the frame id to answer it under.
fn handshake(reader: &mut BufReader<UnixStream>) -> Result<(u64, LaunchParams), (u64, PilotError)> {
    let internal = |id, message: String| (id, PilotError::new(ErrorCode::Internal, message));
    let frame: ClientFrame = match read_line(reader) {
        Ok(Some(frame)) => frame,
        Ok(None) => return Err(internal(0, "the stream ended before hello".into())),
        Err(error) => return Err(internal(0, format!("reading hello: {error}"))),
    };
    match frame.request {
        // A rate outside the measured set is refused here as in the host, before any app starts.
        Request::Hello { version, launch }
            if version == PROTOCOL_VERSION && !REFRESH_RATES_HZ.contains(&launch.refresh_hz) =>
        {
            Err((
                frame.id,
                PilotError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "refresh_hz {} is not one of {REFRESH_RATES_HZ:?}",
                        launch.refresh_hz
                    ),
                ),
            ))
        }
        Request::Hello { version, launch } if version == PROTOCOL_VERSION => Ok((frame.id, launch)),
        Request::Hello { version, .. } => Err((
            frame.id,
            PilotError::new(
                ErrorCode::ProtocolMismatch,
                format!(
                    "the server speaks version {version}, this agent speaks {PROTOCOL_VERSION}"
                ),
            ),
        )),
        _ => Err(internal(frame.id, "the first request must be hello".into())),
    }
}

/// Writes one reply frame; false when the server has gone.
fn answer(writer: &mut UnixStream, id: u64, outcome: Outcome) -> bool {
    match write_line(writer, &BackendFrame::new(id, outcome)) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("gpui-pilot session-agent: writing reply {id}: {error}");
            false
        }
    }
}

/// The app's process, watched and killed through a pidfd so a successor the agent did not spawn
/// is handled the same way; killed when dropped so no failure path leaves it running.
struct App {
    pid: u32,
    pidfd: OwnedFd,
    reaper: Option<JoinHandle<()>>,
}

impl App {
    /// Spawns `command`; a reaper thread owns the `Child` and waits on it, so the app is reaped
    /// the moment it exits rather than lingering as a zombie that `kill -0` still finds. The app
    /// leads a session of its own, so whatever it leaves behind is found by session id.
    fn spawn(command: &mut Command) -> std::io::Result<App> {
        // SAFETY: setsid is a single raw syscall, async-signal-safe between fork and exec.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid()?;
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let pid = child.id();
        // Opened before the child can be reaped, so the pid cannot have been reused.
        let opened =
            child_pid(pid).and_then(|raw| Ok((raw, pidfd_open(raw, PidfdFlags::empty())?)));
        let (raw, pidfd) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        // The thread takes the child only if it starts; std hands the closure back on failure
        // by dropping it, so a failed start falls back to killing through the pidfd and
        // waiting by pid.
        let reaper = std::thread::Builder::new()
            .name("app-reaper".into())
            .spawn(move || {
                let _ = child.wait();
            });
        match reaper {
            Ok(reaper) => Ok(App {
                pid,
                pidfd,
                reaper: Some(reaper),
            }),
            Err(error) => {
                let _ = pidfd_send_signal(&pidfd, Signal::KILL);
                let _ = waitpid(Some(raw), WaitOptions::empty());
                Err(error)
            }
        }
    }

    /// Watches a process the agent did not spawn (a successor); never reaped here.
    fn follow(pid: u32) -> std::io::Result<App> {
        let pidfd = pidfd_open(child_pid(pid)?, PidfdFlags::empty())?;
        Ok(App {
            pid,
            pidfd,
            reaper: None,
        })
    }

    fn pid(&self) -> u32 {
        self.pid
    }

    /// True once the process has exited, reaped or not: its pidfd polls readable.
    fn exited(&self) -> bool {
        let mut fds = [PollFd::new(&self.pidfd, PollFlags::IN)];
        let now = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        rustix::event::poll(&mut fds, Some(&now)).is_ok_and(|ready| ready > 0)
    }
}

fn child_pid(pid: u32) -> std::io::Result<Pid> {
    i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| std::io::Error::other(format!("pid {pid} is not a valid pid")))
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = pidfd_send_signal(&self.pidfd, Signal::KILL);
        if let Some(reaper) = self.reaper.take() {
            let _ = reaper.join();
        }
    }
}

/// Pids of the live (not `Z`) processes whose session id is `session`, the first app's pid:
/// what the app left, such as gpui's restart helper or its successor. D-Bus-activated services
/// are orphaned too, but run in dbus-daemon's session. Unparseable lines are skipped.
fn session_members(session: u32, stats: impl IntoIterator<Item = String>) -> Vec<u32> {
    let session = session.to_string();
    stats
        .into_iter()
        .filter_map(|line| {
            // The command name sits in parentheses and may hold either, so the fields (state,
            // ppid, pgrp, session) are read after the last `)`.
            let pid = line.split_once(' ')?.0.parse::<u32>().ok()?;
            let (_, tail) = line.rsplit_once(')')?;
            let fields: Vec<&str> = tail.split_whitespace().take(4).collect();
            matches!(fields[..], [state, _, _, sid] if state != "Z" && sid == session)
                .then_some(pid)
        })
        .collect()
}

/// True when the app left a live process in its session (see `session_members`).
fn left_behind(session: u32, stats: impl IntoIterator<Item = String>) -> bool {
    !session_members(session, stats).is_empty()
}

/// Every readable `/proc/<pid>/stat` line in the sandbox's pid namespace.
fn sandbox_stats() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("stat")).ok())
        .collect()
}

/// Why a request failed: already a wire error, or a session failure still to be classified
/// against whether the app lives.
enum Failure {
    Pilot(PilotError),
    Session(SessionError),
}

impl From<SessionError> for Failure {
    fn from(error: SessionError) -> Failure {
        Failure::Session(error)
    }
}

/// The running app inside the private session. `app` is declared first so it dies before the
/// session it draws on when an agent is dropped.
struct Agent {
    app: App,
    session: Session,
    /// The first app's pid, which is its session id and stays its successors'.
    app_session: u32,
    client: Rect,
    /// The output's refresh read at bring-up, which `frames` reports against.
    refresh_mhz: u32,
}

impl Agent {
    /// Starts the session, spawns the app, fills the output with its window, waits for a
    /// stable tree, checks isolation again and settles; the reply is the `Hello` answer.
    fn bring_up(args: AgentArgs, launch: &LaunchParams) -> Result<(Agent, Reply), SessionError> {
        let output = Output::new(
            launch.width.round() as u32,
            launch.height.round() as u32,
            launch.scale,
        );
        let session = Session::start(output)?;
        // Read before the app starts, so a session without the mode costs no app start.
        let refresh_mhz = session.refresh_mhz()?;
        let app = App::spawn(
            Command::new(&args.program)
                .args(&args.args)
                .args(&launch.args)
                .envs(args.env.iter().map(|(k, v)| (k, v)))
                .env_remove("GPUI_PILOT_SOCKET")
                .current_dir(&launch.data_dir)
                .stdin(Stdio::null()),
        )
        .map_err(|e| SessionError::LaunchFailed {
            stage: "spawn",
            message: format!("spawning {}: {e}", args.program.display()),
        })?;
        let pid = app.pid();
        let mut agent = Agent {
            app,
            session,
            app_session: pid,
            client: Rect::new(0., 0., 0., 0.),
            refresh_mhz,
        };
        agent.session.wait_window(pid, BRING_UP)?;
        agent.adopt(pid, None)?;
        let settled = agent.settle(Duration::from_millis(launch.settle_ms));
        let window = WindowInfo::new(agent.client.w, agent.client.h, output.scale);
        let hello = Reply::Hello {
            version: PROTOCOL_VERSION,
            pid,
            window,
            settled,
            refresh_mhz,
        };
        Ok((agent, hello))
    }

    /// Resizes the app's window so its client area is `width` by `height` logical pixels,
    /// keeping the frame's top-left and any decoration around the client, through KWin's
    /// placement script; nothing moves the pointer. A client that would leave the output is
    /// `InvalidArgument` naming the output's size. Replies with the client size KWin reports,
    /// which the app's minimum size can make differ from the request.
    fn resize(&mut self, width: f32, height: f32, settle_ms: u64) -> Result<Reply, Failure> {
        gpui_pilot_protocol::check_resize(width, height).map_err(|message| {
            Failure::Pilot(PilotError::new(ErrorCode::InvalidArgument, message))
        })?;
        let output = self.session.output();
        let pid = self.app.pid();
        let geometry = self.session.window(pid)?;
        let frame = Rect::new(
            geometry.frame.x,
            geometry.frame.y,
            width + geometry.frame.w - geometry.client.w,
            height + geometry.frame.h - geometry.client.h,
        );
        if frame.x + frame.w > output.width as f32 || frame.y + frame.h > output.height as f32 {
            return Err(Failure::Pilot(PilotError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "a {width}x{height} window does not fit the {}x{} output",
                    output.width, output.height
                ),
            )));
        }
        self.client = self.session.resize(pid, frame)?.client;
        let settled = self.settle(Duration::from_millis(settle_ms));
        let window = WindowInfo::new(self.client.w, self.client.h, output.scale);
        Ok(Reply::Resize { window, settled })
    }

    /// Places `pid`'s window over the output, waits until `deadline` for a stable tree, reads
    /// the client rect and checks isolation; bring-up and `follow` share it. Without a deadline
    /// the tree gets `BRING_UP` from when placement returns, as bring-up always has.
    fn adopt(&mut self, pid: u32, deadline: Option<Instant>) -> Result<(), SessionError> {
        let output = self.session.output();
        let frame = Rect::new(0., 0., output.width as f32, output.height as f32);
        self.session.place(pid, frame)?;
        self.wait_stable_tree(deadline.unwrap_or_else(|| Instant::now() + BRING_UP))?;
        self.client = self.session.window(pid)?.client;
        self.session.check_isolation()
    }

    /// After the app exited, adopts the successor a self-restart left: while a process the app
    /// left behind lives, polls for a newer pid's window among those processes, all within one
    /// `BRING_UP` deadline. False at once when nothing was left behind, so a crash costs no wait.
    fn follow(&mut self) -> bool {
        let deadline = Instant::now() + BRING_UP;
        loop {
            let members = session_members(self.app_session, sandbox_stats());
            if members.is_empty() || Instant::now() >= deadline {
                return false;
            }
            match self.session.newer_window(self.app.pid(), &members) {
                Ok(Some(successor)) => return self.take_over(successor, deadline),
                Ok(None) => {}
                Err(error) => {
                    eprintln!("gpui-pilot session-agent: looking for a successor: {error}")
                }
            }
            std::thread::sleep(TICK);
        }
    }

    /// Watches `successor` instead of the exited app and adopts its window; the old `App` stays
    /// until the pidfd opens, so a successor already gone leaves the agent to exit 1.
    fn take_over(&mut self, successor: u32, deadline: Instant) -> bool {
        eprintln!(
            "gpui-pilot session-agent: following pid {successor} after pid {} exited",
            self.app.pid()
        );
        match App::follow(successor) {
            Ok(app) => {
                self.app = app;
            }
            Err(error) => {
                eprintln!("gpui-pilot session-agent: watching pid {successor}: {error}");
                return false;
            }
        }
        match self.adopt(successor, Some(deadline)) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("gpui-pilot session-agent: adopting pid {successor}: {error}");
                false
            }
        }
    }

    /// Waits until the tree reads twice, 50 ms apart, with equal bounds: GPUI lays out again a
    /// frame or more after the placement resize. `LaunchFailed` stage `atspi` at `deadline`.
    fn wait_stable_tree(&mut self, deadline: Instant) -> Result<(), SessionError> {
        let mut last: Option<Vec<Rect>> = None;
        loop {
            let why = match self.session.tree(self.app.pid()) {
                Ok(nodes) => {
                    let bounds: Vec<Rect> = nodes.into_iter().map(|n| n.bounds).collect();
                    if last.as_ref() == Some(&bounds) {
                        return Ok(());
                    }
                    last = Some(bounds);
                    "the bounds kept changing".to_owned()
                }
                Err(error) => error.to_string(),
            };
            if Instant::now() >= deadline {
                return Err(SessionError::LaunchFailed {
                    stage: "atspi",
                    message: format!("no stable tree within 15 s: {why}"),
                });
            }
            std::thread::sleep(TICK);
        }
    }

    /// Captures the client area every 50 ms until the pixels hold unchanged for `QUIET`, within
    /// `budget`. A zero budget answers false without capturing.
    fn settle(&mut self, budget: Duration) -> bool {
        if budget.is_zero() {
            return false;
        }
        let deadline = Instant::now() + budget;
        let mut quiet = Quiet::new(QUIET);
        loop {
            match self.session.capture() {
                Ok(capture) => {
                    // Stamped when the capture returns, so a slow capture still counts its span.
                    let at = Instant::now();
                    let mut hasher = DefaultHasher::new();
                    capture.crop(&self.client).rgba.hash(&mut hasher);
                    if quiet.observe(at, hasher.finish()) {
                        return true;
                    }
                }
                Err(error) => eprintln!("gpui-pilot session-agent: settle capture: {error}"),
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(TICK);
        }
    }

    /// Answers requests in order until `Close` or end of stream (exit 0). An app that has
    /// exited, a failure `request_error` refuses to answer, or a lost server exits 1.
    fn serve(mut self, mut reader: BufReader<UnixStream>, mut writer: UnixStream) -> ExitCode {
        loop {
            let frame: ClientFrame = match read_line_lenient(&mut reader) {
                Ok(Some(Ok(frame))) => frame,
                // A request this agent cannot parse is refused and skipped, not fatal.
                Ok(Some(Err(bad))) => {
                    let error = PilotError::new(
                        ErrorCode::InvalidArgument,
                        format!(
                            "this agent (protocol {PROTOCOL_VERSION}) cannot read the request: \
                             {}; it may need a newer agent",
                            bad.message
                        ),
                    );
                    if !answer(&mut writer, bad.id.unwrap_or(0), Outcome::Err(error)) {
                        return ExitCode::from(1);
                    }
                    continue;
                }
                Ok(None) => return self.close(),
                Err(error) => {
                    eprintln!("gpui-pilot session-agent: reading a request: {error}");
                    return ExitCode::from(1);
                }
            };
            if self.app.exited() {
                eprintln!("gpui-pilot session-agent: the app has exited");
                // A close during a restart in flight ends cleanly rather than waiting for it.
                if matches!(frame.request, Request::Close)
                    && left_behind(self.app_session, sandbox_stats())
                {
                    answer(&mut writer, frame.id, Outcome::Ok(Reply::Closed));
                    return self.close();
                }
                if matches!(frame.request, Request::Close) || !self.follow() {
                    return ExitCode::from(1);
                }
            }
            if matches!(frame.request, Request::Close) {
                answer(&mut writer, frame.id, Outcome::Ok(Reply::Closed));
                return self.close();
            }
            if let Request::CloseWindow {
                timeout_ms,
                settle_ms,
            } = frame.request
            {
                // A window that went (or an app that exited with it) ends the agent as `Close` does.
                let closed = match self
                    .session
                    .close_window(self.app.pid(), Duration::from_millis(timeout_ms))
                {
                    Ok(closed) => closed || self.app.exited(),
                    Err(error) => match request_error(error, self.app.exited()) {
                        Some(error) => {
                            if !answer(&mut writer, frame.id, Outcome::Err(error)) {
                                return ExitCode::from(1);
                            }
                            continue;
                        }
                        None => return ExitCode::from(1),
                    },
                };
                let settled = closed || self.settle(Duration::from_millis(settle_ms));
                let reply = Reply::CloseWindow { closed, settled };
                if !answer(&mut writer, frame.id, Outcome::Ok(reply)) {
                    return ExitCode::from(1);
                }
                if closed {
                    return self.close();
                }
                continue;
            }
            let outcome = match self.handle(frame.request) {
                Ok(reply) => Outcome::Ok(reply),
                Err(Failure::Pilot(error)) => Outcome::Err(error),
                Err(Failure::Session(error)) => {
                    // An app that exited mid-request is followed first; the caller then gets
                    // `NotRunning` and can retry against the successor.
                    let exited = self.app.exited();
                    match request_error(error, exited && !self.follow()) {
                        Some(error) => Outcome::Err(error),
                        None => return ExitCode::from(1),
                    }
                }
            };
            if !answer(&mut writer, frame.id, outcome) {
                return ExitCode::from(1);
            }
        }
    }

    fn handle(&mut self, request: Request) -> Result<Reply, Failure> {
        let ms = Duration::from_millis;
        match request {
            Request::Click {
                target,
                button,
                count,
                modifiers,
                timeout_ms,
                settle_ms,
            } => {
                let acted = self.act(&target, timeout_ms, true, |session, pid, at| {
                    session.click(pid, at, button, count, &modifiers)
                })?;
                let settled = self.settle(ms(settle_ms));
                Ok(Reply::Click {
                    clicked: acted.node.clone(),
                    hit: acted.node,
                    settled,
                })
            }
            Request::Hover {
                target,
                timeout_ms,
                settle_ms,
            } => {
                let acted = self.act(&target, timeout_ms, false, Session::hover)?;
                // Wait out the tooltip delay before settling, as the headless host does.
                std::thread::sleep(ms(hover_wait_ms(settle_ms, self.refresh_mhz)));
                let settled = self.settle(ms(settle_ms));
                Ok(Reply::Hover {
                    hovered: acted.node.clone(),
                    hit: acted.node,
                    settled,
                })
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
                match wheel {
                    Some(wheel) => {
                        // The session trusts its counts, so the limits are enforced here,
                        // before any input; `unsigned_abs` keeps `i32::MIN` out too.
                        if wheel.notches_x.unsigned_abs() > MAX_NOTCHES
                            || wheel.notches_y.unsigned_abs() > MAX_NOTCHES
                        {
                            return Err(invalid(format!("at most {MAX_NOTCHES} notches per axis")));
                        }
                        if !NOTCH_MS_RANGE.contains(&wheel.notch_ms) {
                            return Err(invalid("`notch_ms` is between 1 and 1000".into()));
                        }
                        self.act(&target, timeout_ms, false, |session, pid, at| {
                            session.notches(pid, at, &wheel, &modifiers)
                        })?;
                    }
                    None => {
                        self.act(&target, timeout_ms, false, |session, pid, at| {
                            session.scroll(pid, at, dx, dy, &modifiers)
                        })?;
                    }
                }
                let settled = self.settle(ms(settle_ms));
                Ok(Reply::Scroll { settled })
            }
            Request::Type {
                text,
                target,
                timeout_ms,
                settle_ms,
            } => {
                // An untypable character must refuse before the focusing click changes anything.
                self.session.check_text(&text)?;
                if let Some(target) = target {
                    self.act(&target, timeout_ms, true, |session, pid, at| {
                        session.click(pid, at, MouseButton::Left, 1, &[])
                    })?;
                }
                self.session.type_or_paste(&text)?;
                let settled = self.settle(ms(settle_ms));
                // Masking is not read from AT-SPI yet; session callers redact explicitly.
                Ok(Reply::Type {
                    settled,
                    masked: false,
                })
            }
            Request::Key { keys, settle_ms } => {
                self.session.keymap().sequence(&keys)?;
                self.session.keys(&keys)?;
                let settled = self.settle(ms(settle_ms));
                Ok(Reply::Key { settled })
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
                // Resolve the drop end first, only observing it, so a missing target refuses
                // before the source is pressed.
                let end = self.act(&to, timeout_ms, false, |_, _, _| Ok(()))?;
                let end_at = end.at;
                let start = self.act(&from, timeout_ms, true, |session, pid, at| {
                    session.drag(pid, at, end_at, steps.clamp(1, 100), button, &modifiers)
                })?;
                let settled = self.settle(ms(settle_ms));
                Ok(Reply::Drag {
                    from: start.node,
                    to: end.node,
                    settled,
                })
            }
            // A data-device drag has no EIS or portal path into the private session yet.
            Request::Drop { .. } => Err(Failure::Pilot(PilotError::new(
                ErrorCode::Unsupported,
                "drop is headless only for now; session mode cannot drag files into the window",
            ))),
            Request::Resize {
                width,
                height,
                settle_ms,
            } => self.resize(width, height, settle_ms),
            Request::Dialog { .. } => Err(Failure::Pilot(PilotError::new(
                ErrorCode::Unsupported,
                "session mode cannot see GPUI's fallback prompt, which draws no accessible \
                 elements; take a screenshot and click its buttons by point",
            ))),
            Request::Wait { until, timeout_ms } => self.wait(until, timeout_ms),
            Request::Screenshot { target, path } => self.screenshot(target, &path),
            Request::Tree {
                within,
                wait_for,
                timeout_ms,
                settle_ms,
            } => self.tree(within, wait_for, timeout_ms, settle_ms),
            Request::PickFile {
                answer,
                settle_ms,
                timeout_ms,
            } => {
                let timeout = ms(timeout_ms.unwrap_or(PICK_TIMEOUT_MS));
                let answer = self.session.answer_prompt(&answer, timeout)?;
                let settled = self.settle(ms(settle_ms));
                Ok(Reply::PickFile {
                    answered: answer.answered,
                    prompt: Some(answer.prompt),
                    settled,
                })
            }
            Request::Clipboard { set, settle_ms } => {
                let text = self.session.clipboard(set.as_deref())?;
                let settled = self.settle(ms(settle_ms));
                Ok(Reply::Clipboard { text, settled })
            }
            // The session's only `InvalidArgument` here is the recording state, answered in
            // the headless host's words.
            Request::FramesStart { idle_ms } => {
                let refresh_mhz = self.refresh_mhz;
                match self
                    .session
                    .frames_start(self.app.pid(), refresh_mhz, idle_ms)
                {
                    Ok(()) => Ok(Reply::FramesStarted { refresh_mhz }),
                    Err(SessionError::InvalidArgument(_)) => Err(invalid(
                        "frames is already recording; call frames stop first".into(),
                    )),
                    Err(error) => Err(error.into()),
                }
            }
            Request::FramesStop => match self.session.frames_stop() {
                Ok(report) => Ok(Reply::Frames(report)),
                Err(SessionError::InvalidArgument(_)) => Err(invalid(
                    "frames is not recording; call frames start first".into(),
                )),
                Err(error) => Err(error.into()),
            },
            request => Err(Failure::Pilot(PilotError::new(
                ErrorCode::Unsupported,
                format!("{request:?} is not supported in session mode yet"),
            ))),
        }
    }

    /// Polls the tree every 50 ms until `target` is actionable with the same bounds twice in a
    /// row, then runs `action` at its point. A point target acts at once without reading the
    /// tree; a zero timeout acts on the first success; at the deadline an unstable success is
    /// acted on, and a retryable failure keeps its code with `after N ms: ` before its message.
    fn act(
        &mut self,
        target: &Target,
        timeout_ms: u64,
        need_enabled: bool,
        action: impl FnOnce(&mut Session, u32, (f32, f32)) -> Result<(), SessionError>,
    ) -> Result<Actionable, Failure> {
        if let Some(point) = at_point(target) {
            action(&mut self.session, self.app.pid(), point.at)?;
            return Ok(point);
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let once = timeout_ms == 0;
        let mut stable_since: Option<Rect> = None;
        loop {
            let expired = Instant::now() >= deadline;
            let nodes = self.session.tree(self.app.pid())?;
            match actionable(&nodes, target, need_enabled) {
                Ok(a) if once || expired || stable_since.as_ref() == Some(&a.node.bounds) => {
                    action(&mut self.session, self.app.pid(), a.at)?;
                    return Ok(a);
                }
                Ok(a) => stable_since = Some(a.node.bounds),
                Err(error) if !retryable(error.code) || once => return Err(Failure::Pilot(error)),
                Err(mut error) if expired => {
                    error.message = format!("after {timeout_ms} ms: {}", error.message);
                    return Err(Failure::Pilot(error));
                }
                Err(_) => stable_since = None,
            }
            std::thread::sleep(TICK);
        }
    }

    /// Sleeps for `Elapsed`, or polls the tree every 50 ms until `Visible` finds a visible
    /// match (answered as `node`) or `Gone` finds none; running out is `Timeout` with
    /// `after N ms: `. A point target is `Unsupported`.
    fn wait(&mut self, until: WaitUntil, timeout_ms: u64) -> Result<Reply, Failure> {
        let started = Instant::now();
        let waited_ms = || started.elapsed().as_millis() as u64;
        let (target, gone) = match until {
            WaitUntil::Elapsed(ms) => {
                std::thread::sleep(Duration::from_millis(ms));
                return Ok(Reply::Wait {
                    node: None,
                    waited_ms: waited_ms(),
                });
            }
            WaitUntil::Visible(target) => (target, false),
            WaitUntil::Gone(target) => (target, true),
            other => {
                return Err(Failure::Pilot(PilotError::new(
                    ErrorCode::Unsupported,
                    format!("wait until {other:?}"),
                )));
            }
        };
        if let Target::Point { .. } = target {
            return Err(Failure::Pilot(PilotError::new(
                ErrorCode::Unsupported,
                "`wait` takes an element target, not a point",
            )));
        }
        let deadline = started + Duration::from_millis(timeout_ms);
        loop {
            let nodes = self.session.tree(self.app.pid())?;
            let resolved = resolve(&nodes, &target);
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
                Poll::Failed(error) => return Err(Failure::Pilot(error)),
                Poll::Pending(mut timeout) if Instant::now() >= deadline => {
                    timeout.message = format!("after {timeout_ms} ms: {}", timeout.message);
                    return Err(Failure::Pilot(timeout));
                }
                Poll::Pending(_) => std::thread::sleep(TICK),
            }
        }
    }

    /// Captures the client area, or the target element's bounds within it, without settling,
    /// and writes it to `path` as a PNG; the reply is in device pixels.
    fn screenshot(&mut self, target: Option<Target>, path: &Path) -> Result<Reply, Failure> {
        // Element bounds are window-content px, so they move by the client origin on the output.
        let area = match target {
            None => self.client.clone(),
            Some(Target::Point { .. }) => {
                return Err(Failure::Pilot(PilotError::new(
                    ErrorCode::Unsupported,
                    "a screenshot takes an element target, not a point",
                )));
            }
            Some(target) => {
                let nodes = self.session.tree(self.app.pid())?;
                let b = resolve(&nodes, &target).map_err(Failure::Pilot)?.bounds;
                Rect::new(self.client.x + b.x, self.client.y + b.y, b.w, b.h)
            }
        };
        let shot = self.session.capture()?.crop(&area);
        shot.save_png(path)
            .map_err(|e| SessionError::Internal(format!("writing {}: {e}", path.display())))?;
        Ok(Reply::Screenshot {
            w: shot.width,
            h: shot.height,
            scale: shot.scale,
        })
    }

    /// Settles, then polls the tree every 50 ms until `wait_for` resolves; a miss at the
    /// deadline is `Timeout` with the miss's `nearest`, any other failure is returned at once.
    /// The answer is every node, or only `within`'s subtree.
    fn tree(
        &mut self,
        within: Option<Target>,
        wait_for: Option<Target>,
        timeout_ms: u64,
        settle_ms: u64,
    ) -> Result<Reply, Failure> {
        if let Some(Target::Point { .. }) = within {
            return Err(Failure::Pilot(PilotError::new(
                ErrorCode::Unsupported,
                "`within` takes an element target, not a point",
            )));
        }
        self.settle(Duration::from_millis(settle_ms));
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let nodes = loop {
            let nodes = self.session.tree(self.app.pid())?;
            let Some(wanted) = &wait_for else {
                break nodes;
            };
            match resolve(&nodes, wanted) {
                Ok(_) => break nodes,
                Err(error) if error.code != ErrorCode::ElementNotFound => {
                    return Err(Failure::Pilot(error));
                }
                Err(error) if Instant::now() >= deadline => {
                    let mut timeout = PilotError::new(
                        ErrorCode::Timeout,
                        format!("after {timeout_ms} ms: {}", error.message),
                    );
                    timeout.nearest = error.nearest;
                    return Err(Failure::Pilot(timeout));
                }
                Err(_) => std::thread::sleep(TICK),
            }
        };
        let nodes = match &within {
            Some(within) => {
                let root = resolve(&nodes, within).map_err(Failure::Pilot)?;
                crate::resolve::within(&nodes, &root)
            }
            None => nodes,
        };
        // Session dialog state is not wired into `tree` yet; it reports neither field.
        Ok(Reply::Tree {
            nodes,
            prompt: None,
            pick_queued: false,
            dialog: None,
        })
    }

    /// Kills and reaps the app, then stops the session; a stop failure is only logged.
    fn close(self) -> ExitCode {
        let Agent { app, session, .. } = self;
        drop(app);
        if let Err(error) = session.stop() {
            eprintln!("gpui-pilot session-agent: stopping the session: {error}");
        }
        ExitCode::SUCCESS
    }
}

/// Conditions that may clear while the app keeps loading or animating.
fn retryable(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::ElementNotFound | ErrorCode::Disabled)
}

/// One look at the tree during a `wait`: done with the answer's node, still pending with the
/// `Timeout` to return at the deadline, or failed at once.
enum Poll {
    Done(Option<Node>),
    Pending(PilotError),
    Failed(PilotError),
}

/// A visible match ends a `Visible` wait; a hidden one or none keeps it going.
fn visible_poll(resolved: Result<Node, PilotError>) -> Poll {
    match resolved {
        Ok(node) if node.visible => Poll::Done(Some(node)),
        Ok(node) => {
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
    }
}

/// No visible match ends a `Gone` wait: none at all, a hidden one, or several all hidden.
/// Visible matches keep it going and are listed if it times out.
fn gone_poll(resolved: Result<Node, PilotError>) -> Poll {
    let visible: Vec<Node> = match resolved {
        Err(error) if error.code == ErrorCode::ElementNotFound => Vec::new(),
        Err(error) if error.code == ErrorCode::Ambiguous => {
            error.matches.into_iter().filter(|n| n.visible).collect()
        }
        Err(error) => return Poll::Failed(error),
        Ok(node) => Some(node).filter(|n| n.visible).into_iter().collect(),
    };
    if visible.is_empty() {
        return Poll::Done(None);
    }
    let message = format!("{} visible element(s) still match", visible.len());
    let mut timeout = PilotError::new(ErrorCode::Timeout, message);
    timeout.matches = visible;
    Poll::Pending(timeout)
}

/// An `InvalidArgument` refusal with `message`.
fn invalid(message: String) -> Failure {
    Failure::Pilot(PilotError::new(ErrorCode::InvalidArgument, message))
}

/// How a session failure while serving a request reaches the client: `None` once the app has
/// exited (the agent exits 1 without a reply, so the server reports `AppCrashed`), `NotRunning`
/// for `LaunchFailed` stage `window` or `atspi` while the app lives, else the error's own code.
fn request_error(error: SessionError, app_exited: bool) -> Option<PilotError> {
    if app_exited {
        return None;
    }
    Some(match error {
        SessionError::LaunchFailed {
            stage: "window" | "atspi",
            ..
        } => PilotError::new(
            ErrorCode::NotRunning,
            format!("the app lost its window or tree: {error}"),
        ),
        error => error.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_process_in_the_apps_session_counts_as_left_behind() {
        let stats = |lines: &[&str]| lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        // Fields after the name: state, ppid, pgrp, session. Outside processes read session 0.
        let base = [
            "1 (bwrap) S 0 0 0 0",
            "2 (dbus-run-sessio) S 1 0 0 0",
            "3 (dbus-daemon) S 2 0 0 0",
            "4 (gpui-pilot) S 2 0 0 0",
        ];
        assert!(
            !left_behind(47, stats(&base)),
            "only the session's own processes"
        );
        let mut helper = base.to_vec();
        helper.push("52 (bash) S 1 52 47 0");
        assert!(left_behind(47, stats(&helper)), "gpui's restart helper");
        let mut successor = base.to_vec();
        successor.push("53 (pilot-demo) S 52 52 47 0");
        assert!(
            left_behind(47, stats(&successor)),
            "the successor, child of the helper"
        );
        let mut zombie = base.to_vec();
        zombie.push("52 (bash) Z 1 52 47 0");
        assert!(!left_behind(47, stats(&zombie)), "a zombie is not alive");
        let mut activated = base.to_vec();
        activated.push("86 (xdg-desktop-por) S 1 0 0 0");
        assert!(
            !left_behind(47, stats(&activated)),
            "a D-Bus service is orphaned too, but outside the app's session"
        );
        let mut other = base.to_vec();
        other.push("90 (setsid-child) S 1 90 90 0");
        assert!(
            !left_behind(47, stats(&other)),
            "a process in a session of its own"
        );
        let mut odd = base.to_vec();
        odd.push("70 (a) b (c) S 1 70 47 0");
        assert!(
            left_behind(47, stats(&odd)),
            "comm may hold spaces and parens"
        );
        assert!(!left_behind(47, stats(&["garbage"])));
        assert!(
            !left_behind(47, stats(&["52 (bash) S 1 52"])),
            "a truncated line"
        );
    }

    #[test]
    fn the_apps_session_members_are_its_live_processes() {
        let stats = |lines: &[&str]| lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        let lines = stats(&[
            "1 (bwrap) S 0 0 0 0",
            "3 (dbus-daemon) S 2 0 0 0",
            "52 (bash) S 1 52 47 0",
            "53 (pilot-demo) S 52 52 47 0",
            "54 (bash) Z 1 54 47 0",
            "86 (xdg-desktop-por) S 1 0 0 0",
            "90 (setsid-child) S 1 90 90 0",
            "70 (a) b (c) S 1 70 47 0",
            "garbage",
            "x7 (bad pid) S 1 7 47 0",
            "52 (bash) S 1 52",
        ]);
        assert_eq!(session_members(47, lines), vec![52, 53, 70]);
    }

    #[test]
    fn the_app_leads_a_session_of_its_own() {
        let app = App::spawn(Command::new("sleep").arg("5")).unwrap();
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", app.pid())).unwrap();
        assert!(
            left_behind(app.pid(), [stat.clone()]),
            "session id is the app's pid: {stat}"
        );
    }

    #[test]
    fn an_app_that_exits_is_reaped_without_being_asked() {
        let app = App::spawn(Command::new("sh").args(["-c", "exit 3"])).unwrap();
        // A zombie keeps its /proc entry; only a reap removes it.
        let entry = PathBuf::from(format!("/proc/{}", app.pid()));
        let deadline = Instant::now() + Duration::from_secs(2);
        while entry.exists() {
            assert!(
                Instant::now() < deadline,
                "pid {} was never reaped",
                app.pid()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.exited());
    }

    #[test]
    fn a_running_app_is_not_exited_and_dropping_it_kills_and_reaps_it() {
        let app = App::spawn(Command::new("sleep").arg("30")).unwrap();
        let pid = app.pid();
        assert!(!app.exited());
        let started = Instant::now();
        drop(app);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "killed and reaped"
        );
    }

    #[test]
    fn a_followed_process_reports_its_exit_through_the_pidfd() {
        let out = Command::new("sh")
            .args(["-c", "sleep 0.3 >/dev/null 2>&1 & echo $!"])
            .output()
            .unwrap();
        let pid: u32 = String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let followed = App::follow(pid).unwrap();
        assert!(!followed.exited());
        let deadline = Instant::now() + Duration::from_secs(3);
        while !followed.exited() {
            assert!(Instant::now() < deadline, "the exit was never seen");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn following_a_process_that_is_gone_fails() {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(App::follow(pid).is_err());
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn settle_needs_the_same_pixels_for_the_whole_quiet_window() {
        let t0 = Instant::now();
        let mut quiet = Quiet::new(Duration::from_millis(160));
        // A hover frame, then the content 120 ms later: an equal pair 60 ms apart is not enough.
        assert!(!quiet.observe(at(t0, 0), 1));
        assert!(!quiet.observe(at(t0, 60), 2));
        assert!(!quiet.observe(at(t0, 120), 2));
        assert!(!quiet.observe(at(t0, 180), 3));
        assert!(!quiet.observe(at(t0, 240), 3));
        assert!(!quiet.observe(at(t0, 300), 3));
        assert!(quiet.observe(at(t0, 340), 3));
    }

    #[test]
    fn slow_captures_settle_on_their_second_equal_capture() {
        // A dev-built agent captures about every 400 ms; two equal captures already span it.
        let t0 = Instant::now();
        let mut quiet = Quiet::new(Duration::from_millis(160));
        assert!(!quiet.observe(at(t0, 0), 7));
        assert!(quiet.observe(at(t0, 400), 7));
    }

    #[test]
    fn a_change_restarts_the_window() {
        let t0 = Instant::now();
        let mut quiet = Quiet::new(Duration::from_millis(160));
        assert!(!quiet.observe(at(t0, 0), 1));
        assert!(!quiet.observe(at(t0, 150), 2));
        assert!(!quiet.observe(at(t0, 200), 1));
        assert!(!quiet.observe(at(t0, 300), 1));
        assert!(quiet.observe(at(t0, 360), 1));
    }

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn arguments_carry_env_program_and_its_args() {
        let parsed = parse_args(os(&[
            "--env", "A=1", "--env", "B=x=y", "--", "/app", "--boot", "-v",
        ]))
        .unwrap();
        assert_eq!(
            parsed.env,
            vec![("A".into(), "1".into()), ("B".into(), "x=y".into())]
        );
        assert_eq!(parsed.program, PathBuf::from("/app"));
        assert_eq!(parsed.args, os(&["--boot", "-v"]));
        assert_eq!(parse_args(os(&["--", "/app"])).unwrap().env, vec![]);
    }

    #[test]
    fn bad_arguments_are_refused() {
        for bad in [
            &[][..],
            &["/app"],
            &["--env", "NOEQUALS", "--", "/app"],
            &["--env"],
            &["--"],
            &["--bogus", "--", "/app"],
        ] {
            assert!(parse_args(os(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn request_failures_map_to_codes_an_agent_can_act_on() {
        use crate::SessionError;
        use gpui_pilot_protocol::ErrorCode;
        let lost = |stage| SessionError::LaunchFailed {
            stage,
            message: "gone".into(),
        };
        assert_eq!(
            request_error(lost("window"), true),
            None,
            "an exited app ends the agent: AppCrashed"
        );
        assert_eq!(
            request_error(SessionError::Internal("x".into()), true),
            None
        );
        for stage in ["window", "atspi"] {
            assert_eq!(
                request_error(lost(stage), false).unwrap().code,
                ErrorCode::NotRunning,
                "{stage}"
            );
        }
        let script =
            SessionError::Timeout("KWin script gp-geom-3 did not call back within 2 s".into());
        let timeout = request_error(script, false).unwrap();
        assert_eq!(timeout.code, ErrorCode::Timeout);
        assert!(timeout.message.contains("gp-geom-3"), "{}", timeout.message);
        let other = request_error(SessionError::Internal("x".into()), false).unwrap();
        assert_eq!(other.code, ErrorCode::Internal);
    }

    /// Sends one hello at `refresh_hz` through a socket pair and runs the agent's handshake.
    fn hello_at(refresh_hz: u32) -> Result<(u64, LaunchParams), (u64, PilotError)> {
        let (mut server, agent) = UnixStream::pair().unwrap();
        let mut launch = LaunchParams::new(640., 400., 1., "/d".into(), Vec::new());
        launch.refresh_hz = refresh_hz;
        let hello = Request::Hello {
            version: PROTOCOL_VERSION,
            launch,
        };
        write_line(&mut server, &ClientFrame::new(4, hello)).unwrap();
        handshake(&mut BufReader::new(agent))
    }

    #[test]
    fn a_hello_at_an_unmeasured_rate_is_refused_as_the_host_does() {
        let Err((id, error)) = hello_at(75) else {
            panic!("75 Hz was accepted");
        };
        assert_eq!((id, error.code), (4, ErrorCode::InvalidArgument));
        assert!(error.message.contains("75"), "{}", error.message);
        let (id, launch) = hello_at(144).expect("144 Hz is measured");
        assert_eq!((id, launch.refresh_hz), (4, 144));
    }
}
