//! Gated session tests: the skip rule, re-running this test binary inside the sandbox, and the
//! windowed pilot-demo whose stdout reports its state.
#![allow(dead_code)] // each test binary uses a different subset

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};

use gpui_pilot_session::sandbox::{Bind, Network, Output, Sandbox, preflight};

pub const OUTPUT: Output = Output::new(1280, 800, 1.5);

/// True when the session suite should run; prints why not otherwise.
pub fn session_tests() -> bool {
    if std::env::var_os("GPUI_PILOT_SESSION_TESTS").as_deref() != Some("1".as_ref()) {
        eprintln!(
            "skipped: set GPUI_PILOT_SESSION_TESTS=1 to run session tests (they need bwrap, kwin_wayland and a GPU)"
        );
        return false;
    }
    if let Err(error) = preflight(Network::None) {
        panic!("GPUI_PILOT_SESSION_TESTS=1 but the session cannot run: {error}");
    }
    assert!(
        demo_path().exists(),
        "{} is missing; run `cargo build -p pilot-demo --bins`",
        demo_path().display()
    );
    true
}

/// True only inside a sandbox started by `run_inside`.
pub fn inner() -> bool {
    std::env::var_os("GPUI_PILOT_INNER").is_some()
}

pub fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// The cargo profile directory: this test binary sits in its `deps`.
pub fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().to_path_buf()
}

pub fn demo_path() -> PathBuf {
    target_dir().join("pilot-demo-windowed")
}

/// Bounds a whole sandboxed run from the host side, startup included.
const HOST_LIMIT: Duration = Duration::from_secs(150);

/// Runs the ignored test `test` of this binary inside a fresh sandbox and returns its stdout;
/// panics with both streams unless it passed within `HOST_LIMIT`, killing the sandbox then.
pub fn run_inside(test: &str, output: Output, configure: impl FnOnce(&mut Sandbox)) -> String {
    let dir = tempfile::Builder::new()
        .prefix("gp-")
        .tempdir_in("/tmp")
        .unwrap();
    let mut sandbox = Sandbox::new(dir.path(), output);
    sandbox.binds.push(Bind::ro(target_dir()));
    sandbox.vars.push(("GPUI_PILOT_INNER".into(), "1".into()));
    configure(&mut sandbox);
    sandbox.prepare().unwrap();
    // `timeout` inside the sandbox bounds a hung inner test; `HOST_LIMIT` bounds the rest.
    let exe = std::env::current_exe().unwrap();
    let mut args: Vec<OsString> = vec!["120".into(), exe.into()];
    args.extend(
        [
            "--exact",
            test,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]
        .map(OsString::from),
    );
    let timeout = std::path::Path::new("/usr/bin/timeout");
    let mut child = sandbox
        .command(timeout, &args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    let stdout = drain(child.stdout.take().unwrap());
    let stderr = drain(child.stderr.take().unwrap());
    let deadline = Instant::now() + HOST_LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = kill_process_group(Pid::from_child(&child), Signal::KILL);
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (stdout, stderr) = (stdout.join().unwrap(), stderr.join().unwrap());
    let Some(status) = status else {
        panic!("{test} did not finish within {HOST_LIMIT:?}:\n{stdout}\n{stderr}");
    };
    assert!(
        status.success() && stdout.contains("1 passed"),
        "{test} inside the sandbox failed ({status}):\n{stdout}\n{stderr}"
    );
    stdout
}

/// Reads a pipe to its end on a thread, so neither stream can fill and stall the child.
fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// The windowed pilot-demo, its `STATE` lines collected by a reader thread.
pub struct Demo {
    pub child: Child,
    lines: Receiver<String>,
}

impl Demo {
    pub fn spawn() -> Demo {
        Demo::spawn_with(&[])
    }

    /// Spawns the demo with each `(name, value)` pair set in its environment.
    pub fn spawn_with(env: &[(&str, &str)]) -> Demo {
        Demo::spawn_env_args(env, &[])
    }

    /// Spawns the demo with `args` on its command line.
    pub fn spawn_args(args: &[&str]) -> Demo {
        Demo::spawn_env_args(&[], args)
    }

    fn spawn_env_args(env: &[(&str, &str)], args: &[&str]) -> Demo {
        let mut child = Command::new(demo_path())
            .args(args)
            .envs(env.iter().copied())
            .stdout(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(state) = line.strip_prefix("STATE ") {
                    let _ = send.send(state.to_owned());
                }
            }
        });
        Demo { child, lines }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Waits up to 5 s for the state line `want`; panics with every line seen.
    pub fn wait_state(&self, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        loop {
            match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) if line == want => return,
                Ok(line) => seen.push(line),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    panic!("no state {want:?} within 5 s; saw {seen:?}")
                }
            }
        }
    }

    /// Every state line that arrives within `wait`.
    pub fn lines_for(&self, wait: Duration) -> Vec<String> {
        let deadline = Instant::now() + wait;
        let mut seen = Vec::new();
        while let Ok(line) = self
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            seen.push(line);
        }
        seen
    }
}

impl Drop for Demo {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
