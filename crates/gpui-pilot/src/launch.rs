//! Launch preparation: the private launch directory, the host's environment and the build.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{DirBuilder, File, OpenOptions, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use gpui_pilot_protocol::{
    ErrorCode, LaunchParams, MIN_HOST_VERSION, Outcome, PROTOCOL_VERSION, PilotError, Reply,
    Request, WindowInfo,
};
use gpui_pilot_session::sandbox::{self, Bind, Network, Output, Sandbox};
use rustix::fs::OFlags;
use rustix::process::{Pid, Signal, getuid, kill_process_group};

use crate::backend::{Backend, BackendError};
use crate::config::ModeConfig;
use crate::error::ToolError;

/// Lines of build or host output a failure reports.
pub(crate) const TAIL_LINES: usize = 40;
/// Lines of `host.log` quoted in the message when the host exits before connecting.
const EXIT_LOG_LINES: usize = 8;
const EXITED_EARLY: &str = "the host exited before connecting";
/// The longest socket path `sun_path` holds, without its terminating NUL.
pub(crate) const SOCKET_PATH_MAX: usize = 107;

/// The launch directory's socket path would not fit in `sun_path`. `Display` names the path,
/// its length and the limit, and suggests a shorter `TMPDIR`.
#[derive(Debug)]
pub(crate) struct SocketPathTooLong {
    pub path: PathBuf,
}

impl std::fmt::Display for SocketPathTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "socket path {} is {} bytes, over the {SOCKET_PATH_MAX}-byte limit; set a shorter TMPDIR",
            self.path.display(),
            self.path.as_os_str().len(),
        )
    }
}

impl std::error::Error for SocketPathTooLong {}

/// The directory launch directories go under: `tmpdir` (the server's `TMPDIR`) when it is
/// an absolute path, else `/tmp`; a relative or empty value is treated as unset.
pub(crate) fn temp_root(tmpdir: Option<OsString>) -> PathBuf {
    match tmpdir.map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        _ => PathBuf::from("/tmp"),
    }
}

/// `temp/gpui-pilot-<uid>`, created 0700 when missing; an error when it is a symlink, not a
/// directory, or owned by another user.
pub(crate) fn pilot_root(temp: &Path) -> std::io::Result<PathBuf> {
    let uid = getuid().as_raw();
    let root = temp.join(format!("gpui-pilot-{uid}"));
    match DirBuilder::new().mode(0o700).create(&root) {
        Err(error) if error.kind() != ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    // Another user could have planted the name first, so trust only our own real directory.
    let meta = std::fs::symlink_metadata(&root)?;
    if !meta.file_type().is_dir() || meta.uid() != uid {
        return Err(std::io::Error::other(format!(
            "{} is not a directory owned by uid {uid}",
            root.display()
        )));
    }
    Ok(root)
}

/// Creates or truncates `path` with mode 0600, refusing to follow a symlink (`O_NOFOLLOW`).
pub(crate) fn private_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(OFlags::NOFOLLOW.bits() as i32)
        .open(path)
}

/// A private `gp-XXXXXX` directory per launch, removed on drop, and a sibling `gp-XXXXXX`
/// holding the session pick dirs. The sibling lies outside the launch dir's writable bind, so
/// the app can never swap the dirs the server writes into.
pub(crate) struct LaunchDir {
    dir: tempfile::TempDir,
    pick: tempfile::TempDir,
}

impl LaunchDir {
    /// `pilot_root(temp)`, then `remove_stale` on it, then a fresh `gp-XXXXXX` (0700) in it
    /// holding `owner` (a `private_file` with this process's pid) and `home`, `run`, `tmp`
    /// (0700) and `data`, plus a sibling `gp-XXXXXX` with its own `owner` and the pick dirs
    /// `in` and `out` (0700). Before the `gp-` directory is made, a socket path longer than
    /// `SOCKET_PATH_MAX` fails with `ErrorKind::InvalidInput` wrapping `SocketPathTooLong`.
    pub fn create(temp: &Path) -> std::io::Result<LaunchDir> {
        let root = pilot_root(temp)?;
        remove_stale(&root);
        // The random suffix has a fixed width, so a placeholder name measures the real path.
        let socket = root.join("gp-XXXXXX").join("pilot.sock");
        if socket.as_os_str().len() > SOCKET_PATH_MAX {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                SocketPathTooLong { path: socket },
            ));
        }
        // Both dirs carry an owner file, so remove_stale clears either once this process is gone.
        let owned = || -> std::io::Result<tempfile::TempDir> {
            let dir = tempfile::Builder::new()
                .prefix("gp-")
                .rand_bytes(6)
                .permissions(Permissions::from_mode(0o700))
                .tempdir_in(&root)?;
            write!(
                private_file(&dir.path().join("owner"))?,
                "{}",
                std::process::id()
            )?;
            Ok(dir)
        };
        let launch = LaunchDir {
            dir: owned()?,
            pick: owned()?,
        };
        for sub in [
            launch.home(),
            launch.runtime(),
            launch.tmp(),
            launch.data(),
            launch.pick_in(),
            launch.pick_out(),
        ] {
            DirBuilder::new().mode(0o700).create(sub)?;
        }
        Ok(launch)
    }

    /// Prepares a kept dir for another start: refuses (InvalidInput, naming it) a `home`, `data`,
    /// `run` or `tmp` that is not a real directory, removes the old socket, and empties `run` and
    /// `tmp` (recreated 0700). `home`, `data` and the pick dirs keep their content.
    pub fn reuse(&self) -> std::io::Result<()> {
        // The previous app could have swapped a subdir for a link, so check all before removing.
        for sub in [self.home(), self.data(), self.runtime(), self.tmp()] {
            let real = std::fs::symlink_metadata(&sub).is_ok_and(|m| m.file_type().is_dir());
            if !real {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("{} is not a real directory", sub.display()),
                ));
            }
        }
        match std::fs::remove_file(self.socket()) {
            Err(error) if error.kind() != ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        for sub in [self.runtime(), self.tmp()] {
            std::fs::remove_dir_all(&sub)?;
            DirBuilder::new().mode(0o700).create(&sub)?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn socket(&self) -> PathBuf {
        self.path().join("pilot.sock")
    }

    pub fn home(&self) -> PathBuf {
        self.path().join("home")
    }

    pub fn runtime(&self) -> PathBuf {
        self.path().join("run")
    }

    pub fn tmp(&self) -> PathBuf {
        self.path().join("tmp")
    }

    pub fn data(&self) -> PathBuf {
        self.path().join("data")
    }

    pub fn log(&self) -> PathBuf {
        self.path().join("host.log")
    }

    /// Staged pick inputs, bound read-only at the same path inside the sandbox.
    pub fn pick_in(&self) -> PathBuf {
        self.pick.path().join("in")
    }

    /// Redirected save targets, bound writable at the same path inside the sandbox.
    pub fn pick_out(&self) -> PathBuf {
        self.pick.path().join("out")
    }
}

/// Removes direct `gp-*` children of `root` that are real directories owned by this user
/// and whose `owner` file, read with `O_NOFOLLOW`, names a pid without `/proc/<pid>`.
/// Everything else stays. Returns how many it removed.
pub(crate) fn remove_stale(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let uid = getuid().as_raw();
    // Any doubt about an entry (a symlink, a foreign owner, an unreadable pid) keeps it.
    let stale = |path: &Path| -> Option<bool> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        if !meta.file_type().is_dir() || meta.uid() != uid {
            return None;
        }
        let mut owner = String::new();
        OpenOptions::new()
            .read(true)
            .custom_flags(OFlags::NOFOLLOW.bits() as i32)
            .open(path.join("owner"))
            .ok()?
            .read_to_string(&mut owner)
            .ok()?;
        let pid: u32 = owner.trim().parse().ok()?;
        Some(!Path::new(&format!("/proc/{pid}")).exists())
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("gp-"))
        .map(|entry| entry.path())
        .filter(|path| stale(path) == Some(true) && std::fs::remove_dir_all(path).is_ok())
        .count()
}

/// The host's whole environment (decision 4), from the server's `inherited` variables.
pub(crate) fn host_env(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    dir: &LaunchDir,
) -> Vec<(OsString, OsString)> {
    let inherited: Vec<_> = inherited.into_iter().collect();
    let pick = |key: &str| {
        inherited
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let mut env = vec![(
        "PATH".into(),
        pick("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
    )];
    for key in ["LANG", "RUST_LOG", "RUST_BACKTRACE"] {
        if let Some(value) = pick(key) {
            env.push((key.into(), value));
        }
    }
    let home = dir.home();
    let private = [
        ("HOME", home.clone()),
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("XDG_DATA_HOME", home.join(".local/share")),
        ("XDG_CACHE_HOME", home.join(".cache")),
        ("XDG_STATE_HOME", home.join(".local/state")),
        ("XDG_RUNTIME_DIR", dir.runtime()),
        ("TMPDIR", dir.tmp()),
        ("GPUI_PILOT_SOCKET", dir.socket()),
    ];
    env.extend(private.map(|(key, path)| (key.into(), path.into_os_string())));
    env
}

/// The caller's extra app variables in key order. A reserved name, an empty name, a name
/// holding `=` or NUL, or a value holding NUL is `InvalidArgument`.
pub(crate) fn app_env(env: &BTreeMap<String, String>) -> Result<Vec<(String, String)>, ToolError> {
    let invalid = |key: &str, why: &str| {
        ToolError::new(ErrorCode::InvalidArgument, format!("env {key:?}: {why}"))
    };
    env.iter()
        .map(|(key, value)| {
            if key.is_empty() || key.contains(['=', '\0']) {
                return Err(invalid(key, "not a valid variable name"));
            }
            // The sandbox's isolation names, plus the handshake socket the host owns.
            if key == "GPUI_PILOT_SOCKET" || sandbox::is_reserved(key) {
                return Err(invalid(key, "reserved by the pilot's isolation"));
            }
            if value.contains('\0') {
                return Err(invalid(key, "the value holds a NUL byte"));
            }
            Ok((key.clone(), value.clone()))
        })
        .collect()
}

/// The last `n` lines of `text`, joined by newlines.
pub(crate) fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The last `TAIL_LINES` lines of `host.log`, lossily decoded; empty when there is none.
pub(crate) fn log_tail(dir: &LaunchDir) -> String {
    std::fs::read(dir.log())
        .map(|bytes| tail(&String::from_utf8_lossy(&bytes), TAIL_LINES))
        .unwrap_or_default()
}

/// Whether the server's own executable path (from `current_exe`) was replaced or removed
/// since it started: Linux appends " (deleted)" to `/proc/self/exe`, or the path is gone.
/// The session sandbox binds this path, so a stale one would fail inside bwrap.
pub(crate) fn exe_was_replaced(exe: &Path) -> bool {
    exe.as_os_str().as_encoded_bytes().ends_with(b" (deleted)") || !exe.exists()
}

/// Runs `argv` in `root`; empty is no build. A spawn error or a non-zero exit is
/// `BuildFailed` with detail `log_tail`. Returns the elapsed time.
pub(crate) fn run_build(argv: &[String], root: &Path) -> Result<Duration, ToolError> {
    let Some((program, args)) = argv.split_first() else {
        return Ok(Duration::ZERO);
    };
    let failed = |message: String, output: &str| {
        ToolError::new(ErrorCode::BuildFailed, message).with("log_tail", tail(output, TAIL_LINES))
    };
    let start = Instant::now();
    // One pipe for both streams keeps their interleaving in the tail.
    let (mut reader, writer) =
        std::io::pipe().map_err(|e| failed(format!("could not open a pipe: {e}"), ""))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(
            writer
                .try_clone()
                .map_err(|e| failed(format!("could not open a pipe: {e}"), ""))?,
        )
        .stderr(writer);
    let child = command.spawn();
    // The Command holds the pipe's write ends; drop it so the read sees end of stream.
    drop(command);
    let mut child = child.map_err(|e| failed(format!("could not start `{program}`: {e}"), ""))?;
    let mut output = Vec::new();
    let read = reader.read_to_end(&mut output);
    let status = child.wait();
    let output = String::from_utf8_lossy(&output);
    match (read, status) {
        (Ok(_), Ok(status)) if status.success() => Ok(start.elapsed()),
        (_, Ok(status)) => Err(failed(
            format!("build `{}` exited with {status}", argv.join(" ")),
            &output,
        )),
        (_, Err(e)) => Err(failed(
            format!("could not wait for the build: {e}"),
            &output,
        )),
    }
}

/// How long a launch waits at each stage of the handshake.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timeouts {
    /// From spawn until the host connects to the socket.
    pub socket: Duration,
    /// From connect until the `Hello` reply, which follows the app's `open`.
    pub hello: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            socket: Duration::from_secs(10),
            hello: Duration::from_secs(30),
        }
    }
}

/// A child leading its own process group; dropping it kills the whole group and reaps the
/// child, so neither the host nor anything it started outlives the server's hold on it.
pub(crate) struct ProcessGroup(Child);

impl ProcessGroup {
    /// Takes a child spawned with `process_group(0)`.
    pub fn new(child: Child) -> ProcessGroup {
        ProcessGroup(child)
    }

    pub fn child(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let _ = kill_process_group(Pid::from_child(&self.0), Signal::KILL);
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A connected host that has answered `Hello`.
pub(crate) struct Host {
    pub group: ProcessGroup,
    pub backend: Backend,
    pub pid: u32,
    /// Whether the app settled before the host answered `Hello`.
    pub settled: bool,
    /// The rate the app's display really runs at, in millihertz, from `Hello`.
    pub refresh_mhz: u32,
    /// The protocol the `Hello` negotiated: `PROTOCOL_VERSION`, or an older host's.
    pub version: u32,
}

/// The headless host: `section.binary` with `section.args` then `params.args`, env_clear plus
/// `host_env` then `env`, in the launch directory.
pub(crate) fn host_command(
    section: &ModeConfig,
    root: &Path,
    dir: &LaunchDir,
    params: &LaunchParams,
    env: &[(String, String)],
) -> Command {
    let mut command = Command::new(root.join(&section.binary));
    command
        .args(&section.args)
        .args(&params.args)
        .env_clear()
        .envs(host_env(std::env::vars_os(), dir))
        .envs(env.iter().map(|(key, value)| (key, value)))
        .current_dir(dir.path());
    command
}

/// `exe session-agent [--env K=V].. -- <root/binary> <section.args>..` wrapped by a `Sandbox`
/// on `dir` with `output`, `network` and pasta's `host_ports`. Binds: `exe` and the binary read-only, then each
/// `section.binds` entry through `Bind::parse`, then `dir.pick_in()` read-only and
/// `dir.pick_out()` writable. Vars: `GPUI_PILOT_SOCKET=dir.socket()`. `Sandbox::prepare` runs here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn session_command(
    section: &ModeConfig,
    root: &Path,
    dir: &LaunchDir,
    exe: &Path,
    output: Output,
    network: Network,
    host_ports: &[u16],
    env: &[(String, String)],
) -> std::io::Result<Command> {
    let binary = root.join(&section.binary);
    let mut sandbox = Sandbox::new(dir.path(), output);
    sandbox.network = network;
    sandbox.host_ports = host_ports.to_vec();
    // The pick dirs are bound last, at their host paths, so a path the server returns names
    // the same file inside; files the server adds later still show inside.
    sandbox.binds = [Bind::ro(exe), Bind::ro(&binary)]
        .into_iter()
        .chain(section.binds.iter().map(|spec| Bind::parse(spec)))
        .chain([Bind::ro(dir.pick_in()), Bind::rw(dir.pick_out())])
        .collect();
    sandbox.vars = vec![(
        "GPUI_PILOT_SOCKET".to_owned(),
        dir.socket().display().to_string(),
    )];
    sandbox.prepare()?;
    // The agent takes the app's variables as flags, since the sandbox clears the environment.
    let mut args: Vec<OsString> = vec!["session-agent".into()];
    for (key, value) in env {
        args.push("--env".into());
        args.push(format!("{key}={value}").into());
    }
    args.push("--".into());
    args.push(binary.into_os_string());
    args.extend(section.args.iter().map(OsString::from));
    let mut command = sandbox.command(exe, &args);
    command.current_dir(dir.path());
    Ok(command)
}

/// Spawns `section.binary` through `host_command` and hands it to `start_host`. A host built
/// for an older protocol refuses our `Hello`, names its own version and exits, so a
/// `ProtocolMismatch` is retried once with a fresh host at that version when the server still
/// serves it (`retry_version`); any other refusal is returned as it came.
pub(crate) fn spawn_host(
    section: &ModeConfig,
    root: &Path,
    dir: &LaunchDir,
    params: LaunchParams,
    env: &[(String, String)],
    timeouts: Timeouts,
) -> Result<(Host, WindowInfo), ToolError> {
    let command = host_command(section, root, dir, &params, env);
    match start_host(command, dir, params.clone(), PROTOCOL_VERSION, timeouts) {
        Err(error) if error.error.code == ErrorCode::ProtocolMismatch => {
            let Some(version) = retry_version(&error.error.message) else {
                return Err(error);
            };
            // The refused host left its socket file behind; the retry binds the path again.
            let _ = std::fs::remove_file(dir.socket());
            let command = host_command(section, root, dir, &params, env);
            start_host(command, dir, params, version, timeouts)
        }
        started => started,
    }
}

/// The older protocol a host named in its `ProtocolMismatch` refusal ("..., this host speaks
/// N", worded so by every host since protocol 7), when the server still serves it.
fn retry_version(refusal: &str) -> Option<u32> {
    let (_, named) = refusal.rsplit_once("this host speaks ")?;
    let version = named.trim().parse().ok()?;
    (MIN_HOST_VERSION..PROTOCOL_VERSION)
        .contains(&version)
        .then_some(version)
}

/// Spawns `command` with output to `host.log` (a `private_file`), `process_group(0)` and
/// stdin null; waits for it to connect and sends `Hello { version, launch:
/// params }`. Failures before the reply are `LaunchFailed` with `log_tail`; an error reply
/// (such as `ProtocolMismatch`) passes through with `log_tail` added. The child is killed
/// with its group on every failure.
pub(crate) fn start_host(
    mut command: Command,
    dir: &LaunchDir,
    params: LaunchParams,
    version: u32,
    timeouts: Timeouts,
) -> Result<(Host, WindowInfo), ToolError> {
    let failed = |message: String| {
        ToolError::new(ErrorCode::LaunchFailed, message).with("log_tail", log_tail(dir))
    };
    // Listening before the spawn means the host can never connect ahead of the socket.
    let listener = UnixListener::bind(dir.socket())
        .and_then(|listener| listener.set_nonblocking(true).map(|()| listener))
        .map_err(|e| failed(format!("cannot listen on {}: {e}", dir.socket().display())))?;
    let log = private_file(&dir.log())
        .map_err(|e| failed(format!("cannot create {}: {e}", dir.log().display())))?;
    let log_err = log
        .try_clone()
        .map_err(|e| failed(format!("cannot share {}: {e}", dir.log().display())))?;
    let child = command
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .process_group(0)
        .spawn()
        .map_err(|e| {
            let program = Path::new(command.get_program()).display().to_string();
            failed(format!("cannot start {program}: {e}"))
        })?;
    // From here every early return drops the group, which kills and reaps the host.
    let mut group = ProcessGroup::new(child);
    match connect(&listener, group.child(), params, version, timeouts) {
        Ok((
            backend,
            Hello {
                pid,
                window,
                settled,
                refresh_mhz,
            },
        )) => Ok((
            Host {
                group,
                backend,
                pid,
                settled,
                refresh_mhz,
                version,
            },
            window,
        )),
        Err(Connect::Failed(message)) => {
            // The sandbox's own error (such as a missing bind source) lands only in host.log, so
            // an early exit carries its last lines in the message, not just in `log_tail`.
            let log = tail(&log_tail(dir), EXIT_LOG_LINES);
            if message.starts_with(EXITED_EARLY) && !log.is_empty() {
                Err(failed(format!("{message}\n{log}")))
            } else {
                Err(failed(message))
            }
        }
        Err(Connect::Refused(error)) => {
            Err(ToolError::from(*error).with("log_tail", log_tail(dir)))
        }
    }
}

/// Why a handshake ended without a host: a launch failure's message, or the host's own error.
enum Connect {
    Failed(String),
    Refused(Box<PilotError>),
}

/// What the host's `Hello` reply reports.
struct Hello {
    pid: u32,
    window: WindowInfo,
    settled: bool,
    refresh_mhz: u32,
}

/// Accepts the host's connection and exchanges `Hello`, watching for an early exit.
fn connect(
    listener: &UnixListener,
    child: &mut Child,
    params: LaunchParams,
    version: u32,
    timeouts: Timeouts,
) -> Result<(Backend, Hello), Connect> {
    // Poll the non-blocking accept so a host that dies early fails now, not at the timeout.
    let deadline = Instant::now() + timeouts.socket;
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => return Err(Connect::Failed(format!("stage socket: {e}"))),
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(Connect::Failed(format!("{EXITED_EARLY}: {status}")));
        }
        if Instant::now() >= deadline {
            return Err(Connect::Failed(format!(
                "stage socket: no connection within {:?}",
                timeouts.socket
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let hello = |why: String| Connect::Failed(format!("stage hello: {why}"));
    let mut backend = stream
        .set_nonblocking(false)
        .and_then(|()| Backend::new(stream))
        .map_err(|e| hello(e.to_string()))?;
    let request = Request::Hello {
        version,
        launch: params,
    };
    match backend.call(request, timeouts.hello) {
        Ok(Outcome::Ok(Reply::Hello {
            pid,
            window,
            settled,
            refresh_mhz,
            ..
        })) => Ok((
            backend,
            Hello {
                pid,
                window,
                settled,
                refresh_mhz,
            },
        )),
        Ok(Outcome::Err(error)) => Err(Connect::Refused(Box::new(error))),
        Ok(other) => Err(hello(format!("unexpected reply {other:?}"))),
        Err(BackendError::Closed) => match child.try_wait() {
            Ok(Some(status)) => Err(hello(format!("the host exited: {status}"))),
            _ => Err(hello("the host closed the socket".into())),
        },
        Err(BackendError::Timeout) => Err(hello(format!("no reply within {:?}", timeouts.hello))),
        Err(BackendError::Malformed(why)) => Err(hello(format!("malformed reply: {why}"))),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_refused_hello_is_retried_at_the_older_version_the_host_names() {
        let refusal = |host: u32| {
            format!("the server speaks version {PROTOCOL_VERSION}, this host speaks {host}")
        };
        assert_eq!(retry_version(&refusal(8)), Some(8));
        assert_eq!(
            retry_version(&refusal(MIN_HOST_VERSION)),
            Some(MIN_HOST_VERSION)
        );
        // Too old, not older, or not a refusal this server can read: no retry.
        assert_eq!(retry_version(&refusal(MIN_HOST_VERSION - 1)), None);
        assert_eq!(retry_version(&refusal(PROTOCOL_VERSION)), None);
        assert_eq!(retry_version(&refusal(PROTOCOL_VERSION + 1)), None);
        assert_eq!(retry_version("version mismatch"), None);
    }

    fn os(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    // Launch directories go under /tmp so a long caller TMPDIR cannot overflow the socket path.
    pub(crate) fn short_temp() -> tempfile::TempDir {
        tempfile::Builder::new().tempdir_in("/tmp").unwrap()
    }

    #[test]
    fn a_reused_launch_dir_keeps_home_and_data_and_empties_run_and_tmp() {
        let parent = short_temp();
        let dir = LaunchDir::create(parent.path()).unwrap();
        std::fs::write(dir.home().join("settings"), "kept").unwrap();
        std::fs::write(dir.data().join("clicks"), "2").unwrap();
        std::fs::write(dir.runtime().join("wayland-0.lock"), "").unwrap();
        std::fs::create_dir(dir.tmp().join("scratch")).unwrap();
        std::fs::write(dir.socket(), "").unwrap();
        dir.reuse().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.home().join("settings")).unwrap(),
            "kept"
        );
        assert_eq!(
            std::fs::read_to_string(dir.data().join("clicks")).unwrap(),
            "2"
        );
        assert_eq!(std::fs::read_dir(dir.runtime()).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(dir.tmp()).unwrap().count(), 0);
        assert!(!dir.socket().exists());
        for sub in [dir.runtime(), dir.tmp()] {
            let mode = std::fs::metadata(&sub).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", sub.display());
        }
    }

    #[test]
    fn a_reused_launch_dir_refuses_a_symlinked_subdir() {
        let parent = short_temp();
        let dir = LaunchDir::create(parent.path()).unwrap();
        let elsewhere = short_temp();
        std::fs::write(elsewhere.path().join("victim"), "x").unwrap();
        std::fs::remove_dir(dir.runtime()).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), dir.runtime()).unwrap();
        let error = dir.reuse().unwrap_err();
        assert!(error.to_string().contains("run"), "{error}");
        assert!(
            elsewhere.path().join("victim").exists(),
            "nothing behind the link was removed"
        );
    }

    #[test]
    fn the_host_environment_keeps_only_the_allow_list() {
        let parent = short_temp();
        let dir = LaunchDir::create(parent.path()).unwrap();
        let inherited = os(&[
            ("PATH", "/opt/bin:/usr/bin"),
            ("LANG", "C.UTF-8"),
            ("DISPLAY", ":0"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
            ("HOME", "/home/someone"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("SSH_AUTH_SOCK", "/run/user/1000/ssh"),
        ]);
        let env = host_env(inherited, &dir);
        let get = |key: &str| {
            env.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| PathBuf::from(v))
        };
        let mut keys: Vec<_> = env
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "GPUI_PILOT_SOCKET",
                "HOME",
                "LANG",
                "PATH",
                "TMPDIR",
                "XDG_CACHE_HOME",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_RUNTIME_DIR",
                "XDG_STATE_HOME",
            ]
        );
        assert_eq!(get("PATH"), Some("/opt/bin:/usr/bin".into()));
        assert_eq!(get("HOME"), Some(dir.home()));
        assert_eq!(get("XDG_RUNTIME_DIR"), Some(dir.runtime()));
        assert_eq!(get("TMPDIR"), Some(dir.tmp()));
        assert_eq!(get("GPUI_PILOT_SOCKET"), Some(dir.socket()));
        assert!(get("XDG_CONFIG_HOME").unwrap().starts_with(dir.path()));
        let bare = host_env(Vec::new(), &dir);
        assert!(
            bare.iter()
                .any(|(k, v)| k == "PATH" && v == "/usr/bin:/bin")
        );
    }

    #[test]
    fn a_launch_dir_is_private_owned_and_removed_on_drop() {
        let temp = short_temp();
        let dir = LaunchDir::create(temp.path()).unwrap();
        let path = dir.path().to_path_buf();
        let root = temp
            .path()
            .join(format!("gpui-pilot-{}", getuid().as_raw()));
        assert_eq!(path.parent(), Some(root.as_path()));
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("gp-")
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        for private in [root.clone(), path.clone(), dir.runtime(), dir.tmp()] {
            assert_eq!(mode(&private), 0o700, "{}", private.display());
        }
        assert_eq!(mode(&path.join("owner")), 0o600);
        let owner = std::fs::read_to_string(path.join("owner")).unwrap();
        assert_eq!(owner, std::process::id().to_string());
        assert!(dir.socket().as_os_str().len() <= SOCKET_PATH_MAX);
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn a_relative_or_missing_tmpdir_means_tmp() {
        assert_eq!(temp_root(None), PathBuf::from("/tmp"));
        assert_eq!(temp_root(Some("".into())), PathBuf::from("/tmp"));
        assert_eq!(
            temp_root(Some("relative/tmp".into())),
            PathBuf::from("/tmp")
        );
        assert_eq!(
            temp_root(Some("/var/tmp".into())),
            PathBuf::from("/var/tmp")
        );
    }

    #[test]
    fn a_socket_path_over_the_limit_is_refused_by_name() {
        let parent = short_temp();
        let long = parent.path().join("t".repeat(90));
        std::fs::create_dir(&long).unwrap();
        let Err(error) = LaunchDir::create(&long) else {
            panic!("a launch dir under {} was created", long.display());
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let named = error
            .get_ref()
            .and_then(|e| e.downcast_ref::<SocketPathTooLong>());
        assert!(named.is_some(), "{error}");
        let length = named.unwrap().path.as_os_str().len();
        assert!(length > SOCKET_PATH_MAX, "{error}");
        assert!(error.to_string().contains("107"), "{error}");
        assert!(error.to_string().contains(&length.to_string()), "{error}");
        let root = long.join(format!("gpui-pilot-{}", getuid().as_raw()));
        let left = std::fs::read_dir(&root).map(|d| d.count()).unwrap_or(0);
        assert_eq!(left, 0, "no launch directory is left behind");
    }

    #[test]
    fn a_symlinked_pilot_root_is_refused_and_private_files_are_0600() {
        let temp = short_temp();
        let elsewhere = tempfile::tempdir().unwrap();
        let root = temp
            .path()
            .join(format!("gpui-pilot-{}", getuid().as_raw()));
        std::os::unix::fs::symlink(elsewhere.path(), &root).unwrap();
        assert!(LaunchDir::create(temp.path()).is_err());
        let file = elsewhere.path().join("log");
        private_file(&file).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = elsewhere.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(private_file(&link).is_err(), "a symlink is never followed");
    }

    #[test]
    fn stale_launch_dirs_with_a_dead_owner_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let make = |name: &str, owner: Option<String>| {
            let path = root.path().join(name);
            std::fs::create_dir(&path).unwrap();
            if let Some(owner) = owner {
                std::fs::write(path.join("owner"), owner).unwrap();
            }
            path
        };
        let dead = u32::MAX.to_string();
        let stale = make("gp-dead01", Some(dead.clone()));
        let live = make("gp-live01", Some(std::process::id().to_string()));
        let foreign = make("gp-other1", None);
        let unrelated = make("keep-me", Some(dead.clone()));
        // A symlink named like a launch dir, pointing at a victim that looks stale.
        let victim = tempfile::tempdir().unwrap();
        std::fs::write(victim.path().join("owner"), &dead).unwrap();
        std::fs::write(victim.path().join("precious"), "keep").unwrap();
        std::os::unix::fs::symlink(victim.path(), root.path().join("gp-link01")).unwrap();
        // A launch dir whose `owner` is a symlink to a dead pid.
        let linked_owner = make("gp-lnkown", None);
        let pid_file = victim.path().join("pid");
        std::fs::write(&pid_file, &dead).unwrap();
        std::os::unix::fs::symlink(&pid_file, linked_owner.join("owner")).unwrap();

        assert_eq!(remove_stale(root.path()), 1);
        assert!(!stale.exists());
        assert!(live.exists() && foreign.exists() && unrelated.exists() && linked_owner.exists());
        assert!(root.path().join("gp-link01").symlink_metadata().is_ok());
        assert_eq!(
            std::fs::read_to_string(victim.path().join("precious")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn a_build_reports_its_time_or_its_last_lines() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(run_build(&[], root.path()).unwrap(), Duration::ZERO);
        let ok = ["sh", "-c", "echo fine"].map(String::from);
        assert!(run_build(&ok, root.path()).is_ok());
        let failing = ["sh", "-c", "seq 1 50; echo oops >&2; exit 3"].map(String::from);
        let error = run_build(&failing, root.path()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::BuildFailed);
        let tail = error.details["log_tail"].as_str().unwrap().to_owned();
        assert_eq!(tail.lines().count(), TAIL_LINES);
        assert!(
            tail.starts_with("12\n") && tail.ends_with("50\noops"),
            "{tail}"
        );
        let missing = ["/nonexistent/cargo".to_string()];
        assert_eq!(
            run_build(&missing, root.path()).unwrap_err().code(),
            ErrorCode::BuildFailed
        );
    }

    fn params(dir: &LaunchDir) -> LaunchParams {
        LaunchParams::new(640.0, 400.0, 1.0, dir.data(), Vec::new())
    }

    fn section(binary: &str, args: &[&str]) -> ModeConfig {
        ModeConfig {
            binary: binary.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            ..ModeConfig::default()
        }
    }

    #[test]
    fn an_early_exit_puts_the_host_log_tail_in_the_message() {
        let parent = short_temp();
        let dir = LaunchDir::create(parent.path()).unwrap();
        let quick = Timeouts {
            socket: Duration::from_secs(2),
            hello: Duration::from_secs(1),
        };
        let section = section(
            "/bin/sh",
            &["-c", "echo 'bwrap: no source path' >&2; exit 1"],
        );
        let Err(error) = spawn_host(&section, Path::new("/"), &dir, params(&dir), &[], quick)
        else {
            panic!("launched");
        };
        assert!(
            error.error.message.contains("bwrap: no source path"),
            "{}",
            error.error.message
        );
    }

    #[test]
    fn a_replaced_executable_is_recognised_from_its_proc_path() {
        let gone = Path::new("/home/u/.cargo/bin/gpui-pilot (deleted)");
        assert!(exe_was_replaced(gone));
        assert!(exe_was_replaced(Path::new("/nonexistent/gpui-pilot")));
        assert!(!exe_was_replaced(Path::new("/bin/sh")));
    }

    #[test]
    fn a_host_that_never_connects_is_launch_failed() {
        let parent = short_temp();
        let root = Path::new("/");
        let quick = Timeouts {
            socket: Duration::from_millis(300),
            hello: Duration::from_secs(1),
        };
        let cases = [
            (section("/usr/bin/true", &[]), "exited before connecting"),
            (section("/usr/bin/sleep", &["5"]), "stage socket"),
            (section("/nonexistent/pilot", &[]), "cannot start"),
        ];
        for (section, expected) in cases {
            let dir = LaunchDir::create(parent.path()).unwrap();
            let started = Instant::now();
            let Err(error) = spawn_host(&section, root, &dir, params(&dir), &[], quick) else {
                panic!("{} launched", section.binary.display());
            };
            assert_eq!(error.code(), ErrorCode::LaunchFailed);
            assert!(
                error.error.message.contains(expected),
                "{}",
                error.error.message
            );
            assert!(error.details.contains_key("log_tail"));
            if dir.log().exists() {
                let mode = std::fs::metadata(dir.log()).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }
            assert!(started.elapsed() < Duration::from_secs(3));
        }
    }

    #[test]
    fn a_host_refusing_hello_passes_its_error_through() {
        let parent = short_temp();
        let dir = LaunchDir::create(parent.path()).unwrap();
        let listener = UnixListener::bind(dir.socket()).unwrap();
        listener.set_nonblocking(true).unwrap();
        let socket = dir.socket();
        let fake = std::thread::spawn(move || {
            let stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let frame: gpui_pilot_protocol::ClientFrame =
                gpui_pilot_protocol::read_line(&mut reader)
                    .unwrap()
                    .unwrap();
            let error = gpui_pilot_protocol::PilotError::new(
                ErrorCode::ProtocolMismatch,
                "server speaks 1, host speaks 2",
            );
            let reply = gpui_pilot_protocol::BackendFrame::new(frame.id, Outcome::Err(error));
            gpui_pilot_protocol::write_line(&mut writer, &reply).unwrap();
        });
        let mut child = Command::new("/usr/bin/sleep").arg("5").spawn().unwrap();
        let outcome = connect(
            &listener,
            &mut child,
            params(&dir),
            PROTOCOL_VERSION,
            Timeouts::default(),
        );
        child.kill().unwrap();
        child.wait().unwrap();
        fake.join().unwrap();
        let Err(Connect::Refused(error)) = outcome else {
            panic!("expected the host's refusal");
        };
        assert_eq!(error.code, ErrorCode::ProtocolMismatch);
    }

    #[test]
    fn dropping_a_process_group_kills_and_reaps_it_with_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let kid_file = dir.path().join("kid");
        let script = format!("sleep 60 & echo $! > {}; wait", kid_file.display());
        let child = Command::new("/bin/sh")
            .args(["-c", &script])
            .process_group(0)
            .spawn()
            .unwrap();
        let leader = child.id();
        let group = ProcessGroup::new(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        let kid: u32 = loop {
            if let Some(pid) = std::fs::read_to_string(&kid_file)
                .ok()
                .and_then(|t| t.trim().parse().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "the child never started");
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(group);
        // The leader was reaped by the drop: no /proc entry, not even a zombie.
        assert!(!Path::new(&format!("/proc/{leader}")).exists());
        let running = |pid: u32| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|stat| !stat.contains(") Z ") && !stat.contains(") X "))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while running(kid) {
            assert!(
                Instant::now() < deadline,
                "the group's other member survived"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn app_env_passes_plain_names_and_refuses_the_isolation_keys() {
        let env = BTreeMap::from([
            ("RUST_LOG".to_string(), "debug".to_string()),
            ("PILOT_DEMO_GREETING".to_string(), "hi".to_string()),
        ]);
        assert_eq!(
            app_env(&env).unwrap(),
            [
                ("PILOT_DEMO_GREETING".to_string(), "hi".to_string()),
                ("RUST_LOG".to_string(), "debug".to_string()),
            ]
        );
        for key in [
            "HOME",
            "TMPDIR",
            "GPUI_PILOT_SOCKET",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "WAYLAND_SOCKET",
            "AT_SPI_BUS_ADDRESS",
            "DBUS_SESSION_BUS_ADDRESS",
            "DBUS_SYSTEM_BUS_ADDRESS",
            "XDG_CONFIG_HOME",
            "XDG_RUNTIME_DIR",
            "",
            "A=B",
            "NUL\0",
        ] {
            let bad = BTreeMap::from([(key.to_string(), "x".to_string())]);
            assert_eq!(
                app_env(&bad).unwrap_err().code(),
                ErrorCode::InvalidArgument,
                "{key:?}"
            );
        }
        let nul = BTreeMap::from([("OK".to_string(), "a\0b".to_string())]);
        assert_eq!(
            app_env(&nul).unwrap_err().code(),
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn the_session_command_wraps_the_agent_in_the_sandbox() {
        let parent = short_temp();
        let dir = LaunchDir::create(parent.path()).unwrap();
        let section = ModeConfig {
            binary: "target/debug/demo".into(),
            args: vec!["--flag".into()],
            binds: vec!["/srv/fixtures".into()],
            ..ModeConfig::default()
        };
        let env = [("PILOT_DEMO_GREETING".to_owned(), "hi".to_owned())];
        let output = gpui_pilot_session::sandbox::Output::new(1280, 800, 1.5);
        let network = gpui_pilot_session::sandbox::Network::None;
        let command = session_command(
            &section,
            Path::new("/repo"),
            &dir,
            Path::new("/bin/gpui-pilot"),
            output,
            network,
            &[],
            &env,
        )
        .unwrap();
        assert_eq!(command.get_program(), "bwrap");
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let tail = [
            "/bin/gpui-pilot",
            "session-agent",
            "--env",
            "PILOT_DEMO_GREETING=hi",
            "--",
            "/repo/target/debug/demo",
            "--flag",
        ];
        assert!(args.ends_with(&tail.map(String::from)), "{args:?}");
        let runs = |run: &[&str]| args.windows(run.len()).any(|w| w == run);
        assert!(runs(&["--ro-bind", "/bin/gpui-pilot", "/bin/gpui-pilot"]));
        assert!(runs(&[
            "--ro-bind",
            "/repo/target/debug/demo",
            "/repo/target/debug/demo"
        ]));
        assert!(runs(&["--ro-bind", "/srv/fixtures", "/srv/fixtures"]));
        let pick_in = dir.pick_in().display().to_string();
        assert!(runs(&["--ro-bind", &pick_in, &pick_in]), "{args:?}");
        let pick_out = dir.pick_out().display().to_string();
        assert!(runs(&["--bind", &pick_out, &pick_out]), "{args:?}");
        assert!(dir.pick_in().is_dir() && dir.pick_out().is_dir());
        let envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let socket = dir.socket().display().to_string();
        assert!(
            envs.contains(&("GPUI_PILOT_SOCKET".into(), Some(socket))),
            "{envs:?}"
        );
        assert!(
            envs.iter()
                .all(|(k, _)| k != "DISPLAY" && k != "DBUS_SESSION_BUS_ADDRESS")
        );
        assert!(dir.path().join("session.conf").is_file(), "prepare ran");
    }
}
