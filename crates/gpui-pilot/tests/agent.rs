//! The session agent spoken to directly over its socket, inside the sandbox.

use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use gpui_pilot_protocol::{
    BackendFrame, ClientFrame, ErrorCode, LaunchParams, Modifier, Outcome, PROTOCOL_VERSION,
    PickAnswer, PilotError, Rect, Reply, Request, Segment, Target, Wheel, read_line, write_line,
};
use gpui_pilot_session::sandbox::{Bind, Network, Output, Sandbox, preflight};
use rustix::process::{Pid, Signal, kill_process_group};

fn session_tests() -> bool {
    if std::env::var_os("GPUI_PILOT_SESSION_TESTS").as_deref() != Some("1".as_ref()) {
        eprintln!(
            "skipped: set GPUI_PILOT_SESSION_TESTS=1 to run session tests (they need bwrap, kwin_wayland and a GPU)"
        );
        return false;
    }
    if let Err(error) = preflight(Network::None) {
        panic!("GPUI_PILOT_SESSION_TESTS=1 but the session cannot run: {error}");
    }
    true
}

fn id(text: &str) -> Target {
    Target::Id {
        id: vec![Segment::Str(text.to_owned())],
    }
}

fn tree(within: Option<Target>, wait_for: Option<Target>, timeout_ms: u64) -> Request {
    Request::Tree {
        within,
        wait_for,
        timeout_ms,
        settle_ms: 0,
    }
}

/// The agent in a sandbox, launched for pilot-demo-windowed, its stdout's `STATE` lines read
/// by a thread.
struct Agent {
    dir: tempfile::TempDir,
    child: Child,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next: u64,
    states: Receiver<String>,
}

impl Agent {
    fn start(version: u32) -> (Agent, Outcome) {
        Agent::start_with(version, &["PILOT_DEMO_GREETING=hello"])
    }

    /// The agent launched with `env` (each `NAME=VALUE`) passed to the app through `--env`.
    fn start_with(version: u32, env: &[&str]) -> (Agent, Outcome) {
        Agent::start_with_args(version, env, &[])
    }

    /// As `start_with`, with `args` given to the app through the hello's launch args.
    fn start_with_args(version: u32, env: &[&str], args: &[&str]) -> (Agent, Outcome) {
        let dir = tempfile::Builder::new()
            .prefix("gp-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("pilot.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let exe = PathBuf::from(env!("CARGO_BIN_EXE_gpui-pilot"));
        let demo = exe.with_file_name("pilot-demo-windowed");
        assert!(
            demo.exists(),
            "{} is missing; run `cargo build -p pilot-demo --bins`",
            demo.display()
        );
        let mut sandbox = Sandbox::new(dir.path(), Output::new(1280, 800, 1.5));
        sandbox.binds.push(Bind::ro(exe.parent().unwrap()));
        sandbox
            .vars
            .push(("GPUI_PILOT_SOCKET".into(), socket.display().to_string()));
        sandbox.prepare().unwrap();
        let mut agent_args: Vec<OsString> = vec!["session-agent".into()];
        for pair in env {
            agent_args.extend(["--env".into(), OsString::from(*pair)]);
        }
        agent_args.extend(["--".into(), demo.into_os_string()]);
        let mut child = sandbox
            .command(&exe, &agent_args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, states) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(state) = line.strip_prefix("STATE ") {
                    let _ = send.send(state.to_owned());
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(error) => panic!("the agent never connected: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let mut agent = Agent {
            dir,
            child,
            reader,
            writer: stream,
            next: 1,
            states,
        };
        let app_args = args.iter().map(|a| a.to_string()).collect();
        let launch = LaunchParams::new(1280., 800., 1.5, data, app_args);
        let hello = agent.call(Request::Hello { version, launch });
        (agent, hello)
    }

    fn call(&mut self, request: Request) -> Outcome {
        let id = self.next;
        self.next += 1;
        write_line(&mut self.writer, &ClientFrame::new(id, request)).unwrap();
        let frame: BackendFrame = read_line(&mut self.reader)
            .unwrap()
            .expect("the agent closed the socket");
        assert_eq!(frame.id, id);
        frame.outcome
    }

    fn ok(&mut self, request: Request) -> Reply {
        match self.call(request) {
            Outcome::Ok(reply) => reply,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    fn err(&mut self, request: Request) -> PilotError {
        match self.call(request) {
            Outcome::Err(error) => error,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// Waits up to 5 s for the state line `want`; panics with every line seen. Matches by
    /// content: earlier lines and the duplicates the demo prints are skipped, never counted.
    fn wait_state(&self, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        loop {
            match self
                .states
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

    fn wait_exit(&mut self, limit: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = kill_process_group(Pid::from_child(&self.child), Signal::KILL);
        let _ = self.child.wait();
    }
}

#[test]
fn hello_places_the_window_and_close_ends_the_agent() {
    if !session_tests() {
        return;
    }
    let (mut agent, hello) = Agent::start(PROTOCOL_VERSION);
    let Outcome::Ok(Reply::Hello {
        pid,
        window,
        settled,
        refresh_mhz,
        ..
    }) = hello
    else {
        panic!("{hello:?}")
    };
    assert!(pid > 0 && settled, "{pid} {settled}");
    assert_eq!((window.w, window.scale), (1280., 1.5));
    assert!(
        window.h > 700. && window.h < 800.,
        "the client below the title bar: {window:?}"
    );
    assert_eq!(
        refresh_mhz, 60_000,
        "the agent reads back KWin's built-in 60 Hz mode"
    );
    agent.wait_state("opened");
    assert_eq!(agent.ok(Request::Close), Reply::Closed);
    let status = agent
        .wait_exit(Duration::from_secs(10))
        .expect("the agent exits after close");
    assert!(status.success(), "{status}");
}

#[test]
fn another_protocol_version_is_refused_before_the_session_starts() {
    if !session_tests() {
        return;
    }
    let (mut agent, hello) = Agent::start(PROTOCOL_VERSION + 1);
    let Outcome::Err(error) = hello else {
        panic!("{hello:?}")
    };
    assert_eq!(error.code, ErrorCode::ProtocolMismatch);
    assert!(agent.wait_exit(Duration::from_secs(5)).is_some());
}

#[test]
fn an_app_that_opens_a_desktop_socket_fails_the_hello() {
    if !session_tests() {
        return;
    }
    // The app binds the address after the session's own checks passed; only the post-launch
    // check can see it.
    let plant = ["PILOT_DEMO_PLANT_ABSTRACT=/tmp/.X11-unix/X1"];
    let (mut agent, hello) = Agent::start_with(PROTOCOL_VERSION, &plant);
    let Outcome::Err(error) = hello else {
        panic!("{hello:?}")
    };
    assert_eq!(error.code, ErrorCode::IsolationBreach);
    assert!(
        error.message.contains("@/tmp/.X11-unix/X1"),
        "{}",
        error.message
    );
    let status = agent
        .wait_exit(Duration::from_secs(10))
        .expect("the agent exits after a failed hello");
    assert_eq!(status.code(), Some(1));
}

#[test]
fn tree_and_screenshot_read_the_placed_window() {
    if !session_tests() {
        return;
    }
    let (mut agent, hello) = Agent::start(PROTOCOL_VERSION);
    let Outcome::Ok(Reply::Hello { window, .. }) = hello else {
        panic!("{hello:?}")
    };

    let shot = agent.dir.path().join("shot.png");
    let Reply::Screenshot { w, h, scale } = agent.ok(Request::Screenshot {
        target: None,
        path: shot.clone(),
    }) else {
        panic!("not a screenshot")
    };
    assert_eq!((w, scale), (1920, 1.5));
    assert_eq!(h, (window.h * 1.5).round() as u32);
    assert!(std::fs::read(&shot).unwrap().starts_with(b"\x89PNG"));
    let Reply::Screenshot { w, h, .. } = agent.ok(Request::Screenshot {
        target: Some(id("count")),
        path: shot,
    }) else {
        panic!("not a screenshot")
    };
    assert_eq!((w, h), (1872, 48));

    let Reply::Tree { nodes, .. } = agent.ok(tree(None, None, 0)) else {
        panic!("not a tree")
    };
    let count = nodes
        .iter()
        .find(|n| n.id.as_deref() == Some("count"))
        .expect("count");
    assert_eq!(count.bounds, Rect::new(16., 50., 1248., 32.));
    assert!(
        nodes
            .iter()
            .any(|n| n.role.as_deref() == Some("Button") && n.name.as_deref() == Some("hello")),
        "--env reached the app"
    );
    let Reply::Tree { nodes, .. } = agent.ok(tree(Some(id("count")), None, 0)) else {
        panic!("not a tree")
    };
    assert_eq!(nodes.len(), 1);
    let missing = agent.err(tree(None, Some(id("nope")), 300));
    assert_eq!(missing.code, ErrorCode::Timeout);
    assert!(
        missing.message.starts_with("after 300 ms: "),
        "{}",
        missing.message
    );

    let pick = agent.err(Request::PickFile {
        answer: PickAnswer::Cancel,
        settle_ms: 0,
        timeout_ms: Some(300),
    });
    assert_eq!(pick.code, ErrorCode::Timeout, "no dialog is open");

    assert_eq!(agent.ok(Request::Close), Reply::Closed);
}

fn click(target: Target, timeout_ms: u64) -> Request {
    Request::Click {
        target,
        button: gpui_pilot_protocol::MouseButton::Left,
        count: 1,
        modifiers: Vec::new(),
        timeout_ms,
        settle_ms: 2000,
    }
}

#[test]
fn input_tools_reach_the_app_and_refuse_what_they_cannot_do() {
    if !session_tests() {
        return;
    }
    use gpui_pilot_protocol::WaitUntil;
    let (mut agent, hello) = Agent::start(PROTOCOL_VERSION);
    assert!(
        matches!(hello, Outcome::Ok(Reply::Hello { .. })),
        "{hello:?}"
    );

    let Reply::Click {
        clicked,
        hit,
        settled,
    } = agent.ok(click(id("count"), 5000))
    else {
        panic!("not a click")
    };
    assert_eq!((clicked.id.as_deref(), settled), (Some("count"), true));
    assert_eq!(hit, clicked);
    agent.wait_state("clicks=1 name=\"\"");
    let by_role = Target::Role {
        role: "Button".into(),
        name: "Count".into(),
        nth: None,
    };
    agent.ok(click(by_role, 5000));
    agent.wait_state("clicks=2 name=\"\"");

    let typed = agent.ok(Request::Type {
        text: "Hié".into(),
        target: Some(id("name")),
        timeout_ms: 5000,
        settle_ms: 2000,
    });
    assert_eq!(
        typed,
        Reply::Type {
            settled: true,
            masked: false
        }
    );
    agent.wait_state("clicks=2 name=\"Hié\"");
    let pasted = agent.ok(Request::Type {
        text: "漢".into(),
        target: None,
        timeout_ms: 0,
        settle_ms: 2000,
    });
    assert_eq!(
        pasted,
        Reply::Type {
            settled: true,
            masked: false
        }
    );
    agent.wait_state("clicks=2 name=\"Hié漢\"");
    agent.ok(Request::Key {
        keys: "backspace backspace shift-a".into(),
        settle_ms: 2000,
    });
    agent.wait_state("clicks=2 name=\"HiA\"");
    let bad = agent.err(Request::Key {
        keys: "ctrl-a bogus".into(),
        settle_ms: 0,
    });
    assert_eq!(bad.code, ErrorCode::InvalidKeystroke);

    // The pointer rests on `name` since the typing click, so hovering `count` restyles it.
    let plain = agent.dir.path().join("plain.png");
    let lit = agent.dir.path().join("lit.png");
    agent.ok(Request::Screenshot {
        target: Some(id("count")),
        path: plain.clone(),
    });
    let Reply::Hover {
        hovered,
        hit,
        settled,
    } = agent.ok(Request::Hover {
        target: id("count"),
        timeout_ms: 5000,
        settle_ms: 2000,
    })
    else {
        panic!("not a hover")
    };
    assert_eq!((hovered.id.as_deref(), settled), (Some("count"), true));
    assert_eq!(hit, hovered);
    agent.ok(Request::Screenshot {
        target: Some(id("count")),
        path: lit.clone(),
    });
    assert_ne!(
        std::fs::read(&plain).unwrap(),
        std::fs::read(&lit).unwrap(),
        "the hover style repainted count"
    );
    let scrolled = agent.ok(Request::Scroll {
        target: id("count"),
        dx: 0.,
        dy: 40.,
        modifiers: Vec::new(),
        wheel: None,
        timeout_ms: 5000,
        settle_ms: 2000,
    });
    assert_eq!(scrolled, Reply::Scroll { settled: true });
    agent.wait_state("clicks=2 name=\"HiA\" scrolled");

    let missing = agent.err(click(id("nope"), 0));
    assert_eq!(missing.code, ErrorCode::ElementNotFound);
    assert!(
        !missing.message.starts_with("after "),
        "one attempt has no prefix"
    );
    let waited = agent.err(click(id("nope"), 200));
    assert!(
        waited.message.starts_with("after 200 ms: "),
        "{}",
        waited.message
    );

    let Reply::Wait { node, .. } = agent.ok(Request::Wait {
        until: WaitUntil::Visible(id("count")),
        timeout_ms: 2000,
    }) else {
        panic!("not a wait")
    };
    assert_eq!(node.and_then(|n| n.id), Some("count".to_owned()));
    let Reply::Wait { waited_ms, .. } = agent.ok(Request::Wait {
        until: WaitUntil::Elapsed(100),
        timeout_ms: 0,
    }) else {
        panic!("not a wait")
    };
    assert!(waited_ms >= 100);
    let stays = agent.err(Request::Wait {
        until: WaitUntil::Gone(id("count")),
        timeout_ms: 200,
    });
    assert_eq!(stays.code, ErrorCode::Timeout);
    assert_eq!(agent.ok(Request::Close), Reply::Closed);
}

#[test]
fn pick_file_and_clipboard_go_through_the_portal_and_the_compositor() {
    if !session_tests() {
        return;
    }
    use gpui_pilot_protocol::{Answered, PromptKind};
    let (mut agent, hello) = Agent::start(PROTOCOL_VERSION);
    assert!(
        matches!(hello, Outcome::Ok(Reply::Hello { .. })),
        "{hello:?}"
    );
    let input = agent.dir.path().join("input.txt");
    std::fs::write(&input, "fixture").unwrap();
    let button = |name: &str| Target::Role {
        role: "Button".into(),
        name: name.into(),
        nth: None,
    };
    let pick = |answer: PickAnswer| Request::PickFile {
        answer,
        settle_ms: 2000,
        timeout_ms: Some(10_000),
    };

    agent.ok(click(button("Open"), 5000));
    let Reply::PickFile {
        answered,
        prompt,
        settled,
    } = agent.ok(pick(PickAnswer::Paths(vec![input.clone()])))
    else {
        panic!("not a pick")
    };
    assert_eq!((answered, settled), (Answered::Open, true));
    assert_eq!(prompt.map(|p| p.kind), Some(PromptKind::Open));
    agent.wait_state(&format!(
        "clicks=0 name=\"\" opened={:?}",
        vec![input.clone()]
    ));

    agent.ok(click(button("Open"), 5000));
    let Reply::PickFile { answered, .. } = agent.ok(pick(PickAnswer::Cancel)) else {
        panic!("not a pick")
    };
    assert_eq!(answered, Answered::Cancelled);

    let Reply::Clipboard { text, settled } = agent.ok(Request::Clipboard {
        set: Some("héllo ✓".into()),
        settle_ms: 2000,
    }) else {
        panic!("not a clipboard reply")
    };
    assert_eq!((text.as_deref(), settled), (Some("héllo ✓"), true));
    agent.ok(click(button("Paste"), 5000));
    agent.wait_state(&format!(
        "clicks=0 name=\"\" opened={:?} pasted=\"héllo ✓\"",
        vec![input]
    ));
    assert_eq!(agent.ok(Request::Close), Reply::Closed);
}

#[test]
fn clicks_land_in_a_window_with_client_side_decorations() {
    if !session_tests() {
        return;
    }
    // Mirrors Apinata: gpui-component's window border pads the content by its shadow inset.
    let (mut agent, hello) = Agent::start_with(PROTOCOL_VERSION, &["PILOT_DEMO_CSD=1"]);
    assert!(
        matches!(hello, Outcome::Ok(Reply::Hello { .. })),
        "{hello:?}"
    );
    agent.wait_state("opened");

    let Reply::Click { clicked, .. } = agent.ok(click(id("count"), 5000)) else {
        panic!("not a click")
    };
    assert_eq!(clicked.id.as_deref(), Some("count"));
    agent.wait_state("clicks=1 name=\"\"");

    // A point taken from the tree hits the same element.
    let Reply::Tree { nodes, .. } = agent.ok(tree(None, None, 0)) else {
        panic!("not a tree")
    };
    let count = nodes
        .iter()
        .find(|n| n.id.as_deref() == Some("count"))
        .expect("count")
        .bounds
        .clone();
    let centre = Target::Point {
        x: count.x + count.w / 2.,
        y: count.y + count.h / 2.,
    };
    agent.ok(click(centre, 5000));
    agent.wait_state("clicks=2 name=\"\"");
    assert_eq!(agent.ok(Request::Close), Reply::Closed);
}

#[test]
fn drag_reaches_the_app_and_dialog_is_unsupported_in_session_mode() {
    if !session_tests() {
        return;
    }
    let (mut agent, hello) = Agent::start(PROTOCOL_VERSION);
    assert!(
        matches!(hello, Outcome::Ok(Reply::Hello { .. })),
        "{hello:?}"
    );
    agent.wait_state("opened");
    let Reply::Drag { from, to, settled } = agent.ok(Request::Drag {
        from: id("chip"),
        to: id("dropzone"),
        steps: 10,
        button: gpui_pilot_protocol::MouseButton::Left,
        modifiers: Vec::new(),
        timeout_ms: 5000,
        settle_ms: 2000,
    }) else {
        panic!("not a drag")
    };
    assert_eq!(
        (from.id.as_deref(), to.id.as_deref()),
        (Some("chip"), Some("dropzone"))
    );
    assert!(settled);
    agent.wait_state("clicks=0 name=\"\" dropped=1");
    let error = agent.err(Request::Dialog {
        button: None,
        timeout_ms: 0,
        settle_ms: 0,
    });
    assert_eq!(error.code, ErrorCode::Unsupported);
    assert!(error.message.contains("screenshot"), "{}", error.message);
    let Reply::Tree { dialog, .. } = agent.ok(tree(None, None, 0)) else {
        panic!("not a tree")
    };
    assert_eq!(dialog, None);
    assert_eq!(agent.ok(Request::Close), Reply::Closed);
}

#[test]
fn drop_is_unsupported_in_session_mode_and_points_at_headless() {
    if !session_tests() {
        return;
    }
    let (mut agent, hello) = Agent::start(PROTOCOL_VERSION);
    assert!(
        matches!(hello, Outcome::Ok(Reply::Hello { .. })),
        "{hello:?}"
    );
    agent.wait_state("opened");
    let error = agent.err(Request::Drop {
        target: id("files"),
        paths: vec!["/tmp/a".into()],
        cancel: false,
        timeout_ms: 0,
        settle_ms: 0,
    });
    assert_eq!(error.code, ErrorCode::Unsupported);
    assert!(
        error.message.starts_with("drop is headless only"),
        "{}",
        error.message
    );
    assert_eq!(agent.ok(Request::Close), Reply::Closed);
}

#[test]
fn ctrl_scroll_reaches_the_app_with_its_modifier() {
    if !session_tests() {
        return;
    }
    let (mut agent, hello) = Agent::start(PROTOCOL_VERSION);
    assert!(
        matches!(hello, Outcome::Ok(Reply::Hello { .. })),
        "{hello:?}"
    );
    agent.wait_state("opened");
    let scroll = |modifiers| Request::Scroll {
        target: id("name"),
        dx: 0.,
        dy: 15.,
        modifiers,
        wheel: None,
        timeout_ms: 5000,
        settle_ms: 2000,
    };
    assert_eq!(
        agent.ok(scroll(vec![Modifier::Ctrl])),
        Reply::Scroll { settled: true }
    );
    agent.wait_state("clicks=0 name=\"\" zoom=1");
    agent.ok(scroll(Vec::new()));
    agent.wait_state("clicks=0 name=\"\" scrolled zoom=1");
    assert_eq!(agent.ok(Request::Close), Reply::Closed);
}

#[test]
fn modifier_clicks_and_drags_reach_the_session_app_with_their_modifiers() {
    if !session_tests() {
        return;
    }
    let (mut agent, hello) = Agent::start(PROTOCOL_VERSION);
    assert!(
        matches!(hello, Outcome::Ok(Reply::Hello { .. })),
        "{hello:?}"
    );
    agent.wait_state("opened");
    let click = |modifiers| Request::Click {
        target: id("count"),
        button: gpui_pilot_protocol::MouseButton::Left,
        count: 1,
        modifiers,
        timeout_ms: 5000,
        settle_ms: 2000,
    };
    agent.ok(click(vec![Modifier::Ctrl, Modifier::Shift]));
    agent.wait_state("clicks=1 name=\"\" down=ctrl+shift up=ctrl+shift");
    agent.ok(click(Vec::new()));
    agent.wait_state("clicks=2 name=\"\"");
    agent.ok(Request::Drag {
        from: id("chip"),
        to: id("dropzone"),
        steps: 10,
        button: gpui_pilot_protocol::MouseButton::Left,
        modifiers: vec![Modifier::Shift],
        timeout_ms: 5000,
        settle_ms: 2000,
    });
    agent.wait_state("clicks=2 name=\"\" dropped=1 down=shift up=shift");
    assert_eq!(agent.ok(Request::Close), Reply::Closed);
}

#[test]
fn a_right_drag_reaches_the_session_app_with_the_right_button_held() {
    if !session_tests() {
        return;
    }
    let (mut agent, hello) = Agent::start(PROTOCOL_VERSION);
    assert!(
        matches!(hello, Outcome::Ok(Reply::Hello { .. })),
        "{hello:?}"
    );
    agent.wait_state("opened");
    agent.ok(Request::Drag {
        from: id("chip"),
        to: id("dropzone"),
        steps: 10,
        button: gpui_pilot_protocol::MouseButton::Right,
        modifiers: Vec::new(),
        timeout_ms: 5000,
        settle_ms: 2000,
    });
    agent.wait_state("clicks=0 name=\"\" held=right");
    assert_eq!(agent.ok(Request::Close), Reply::Closed);
}

#[test]
fn close_window_keeps_a_guarded_app_and_ends_an_unguarded_one() {
    if !session_tests() {
        return;
    }
    let (mut agent, hello) = Agent::start_with(PROTOCOL_VERSION, &["PILOT_DEMO_GUARD_CLOSE=1"]);
    assert!(
        matches!(hello, Outcome::Ok(Reply::Hello { .. })),
        "{hello:?}"
    );
    agent.wait_state("opened");
    let kept = agent.ok(Request::CloseWindow {
        timeout_ms: 1000,
        settle_ms: 2000,
    });
    assert!(
        matches!(kept, Reply::CloseWindow { closed: false, .. }),
        "{kept:?}"
    );
    agent.wait_state("clicks=0 name=\"\" close_asks=1");
    assert!(matches!(
        agent.ok(tree(None, Some(id("count")), 5000)),
        Reply::Tree { .. }
    ));
    assert_eq!(agent.ok(Request::Close), Reply::Closed);
    drop(agent);

    let (mut agent, _) = Agent::start(PROTOCOL_VERSION);
    agent.wait_state("opened");
    let closed = agent.ok(Request::CloseWindow {
        timeout_ms: 5000,
        settle_ms: 2000,
    });
    assert_eq!(
        closed,
        Reply::CloseWindow {
            closed: true,
            settled: true
        }
    );
    let status = agent
        .wait_exit(Duration::from_secs(10))
        .expect("the agent ends after the window closed");
    assert!(status.success(), "{status}");
}

#[test]
fn frames_record_wheel_notches_in_session_mode() {
    if !session_tests() {
        return;
    }
    let (mut agent, hello) = Agent::start_with_args(PROTOCOL_VERSION, &[], &["--list"]);
    let Outcome::Ok(Reply::Hello { refresh_mhz, .. }) = hello else {
        panic!("{hello:?}")
    };
    agent.wait_state("opened");
    assert_eq!(
        agent.err(Request::FramesStop).code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        agent.ok(Request::FramesStart { idle_ms: 100 }),
        Reply::FramesStarted { refresh_mhz }
    );
    assert_eq!(
        agent.err(Request::FramesStart { idle_ms: 100 }).code,
        ErrorCode::InvalidArgument
    );
    let notches = |wheel| Request::Scroll {
        target: Target::Point { x: 300., y: 200. },
        dx: 0.,
        dy: 0.,
        modifiers: Vec::new(),
        wheel: Some(wheel),
        timeout_ms: 0,
        settle_ms: 0,
    };
    for bad in [
        Wheel::new(0, 201, 16),
        Wheel::new(-201, 0, 16),
        Wheel::new(i32::MIN, 0, 16),
        Wheel::new(0, 3, 0),
        Wheel::new(0, 3, 1001),
    ] {
        assert_eq!(
            agent.err(notches(bad)).code,
            ErrorCode::InvalidArgument,
            "{bad:?}"
        );
    }
    assert_eq!(
        agent.ok(notches(Wheel::new(0, 20, 10))),
        Reply::Scroll { settled: false }
    );
    agent.wait_state("clicks=0 name=\"\" wheels=20 lines=-60");
    std::thread::sleep(Duration::from_millis(100));
    let Reply::Frames(report) = agent.ok(Request::FramesStop) else {
        panic!("a frames reply")
    };
    assert_eq!(report.refresh_mhz, refresh_mhz);
    assert!(report.frames >= 10, "{report:?}");
    assert_eq!(
        report.latency.as_ref().map(|l| l.count),
        Some(20),
        "{report:?}"
    );
    assert!(
        report.work.is_none() && report.build.is_none(),
        "{report:?}"
    );
    assert!(!report.truncated, "{report:?}");
    assert_eq!(
        agent.err(Request::FramesStop).code,
        ErrorCode::InvalidArgument
    );
    drop(agent);

    // A pixel scroll after a recording is still one wheel event; the second start of a
    // recording refuses without ending the first.
    let (mut agent, _) = Agent::start(PROTOCOL_VERSION);
    agent.wait_state("opened");
    agent.ok(Request::FramesStart { idle_ms: 100 });
    assert_eq!(
        agent.err(Request::FramesStart { idle_ms: 100 }).code,
        ErrorCode::InvalidArgument
    );
    let pixels = Request::Scroll {
        target: id("name"),
        dx: 0.,
        dy: 40.,
        modifiers: Vec::new(),
        wheel: None,
        timeout_ms: 5000,
        settle_ms: 2000,
    };
    assert_eq!(agent.ok(pixels), Reply::Scroll { settled: true });
    agent.wait_state("clicks=0 name=\"\" scrolled");
    assert!(
        matches!(agent.ok(Request::FramesStop), Reply::Frames(_)),
        "the first recording survived the refused start"
    );
    assert_eq!(agent.ok(Request::Close), Reply::Closed);
}

#[test]
fn an_app_that_restarts_itself_is_followed_to_its_successor() {
    if !session_tests() {
        return;
    }
    let (mut agent, hello) = Agent::start_with(PROTOCOL_VERSION, &["PILOT_DEMO_RESTART=1"]);
    assert!(
        matches!(hello, Outcome::Ok(Reply::Hello { .. })),
        "{hello:?}"
    );
    agent.wait_state("opened");
    std::fs::write(agent.dir.path().join("data/restart"), "").unwrap();
    agent.wait_state("restarting");
    // gpui's helper starts the successor only once `kill -0` fails on the old pid.
    agent.wait_state("opened");

    let Reply::Tree { nodes, .. } = agent.ok(tree(None, Some(id("count")), 15_000)) else {
        panic!("not a tree")
    };
    assert!(nodes.iter().any(|n| n.id.as_deref() == Some("count")));
    let shot = agent.dir.path().join("successor.png");
    let Reply::Screenshot { w, scale, .. } = agent.ok(Request::Screenshot {
        target: None,
        path: shot,
    }) else {
        panic!("not a screenshot")
    };
    assert_eq!((w, scale), (1920, 1.5), "the successor's window was placed");
    let reply = agent.ok(click(id("count"), 5000));
    assert!(matches!(reply, Reply::Click { .. }), "{reply:?}");
    agent.wait_state("clicks=1 name=\"\"");

    assert_eq!(agent.ok(Request::Close), Reply::Closed);
    let status = agent
        .wait_exit(Duration::from_secs(10))
        .expect("the agent exits after close");
    assert!(status.success(), "{status}");
}

#[test]
fn an_app_that_quits_without_a_successor_still_ends_the_agent_at_once() {
    if !session_tests() {
        return;
    }
    let (mut agent, hello) = Agent::start_with(PROTOCOL_VERSION, &["PILOT_DEMO_RESTART=1"]);
    assert!(
        matches!(hello, Outcome::Ok(Reply::Hello { .. })),
        "{hello:?}"
    );
    agent.wait_state("opened");
    std::fs::write(agent.dir.path().join("data/quit"), "").unwrap();
    agent.wait_state("quitting");
    std::thread::sleep(Duration::from_millis(500));

    let started = Instant::now();
    write_line(
        &mut agent.writer,
        &ClientFrame::new(99, tree(None, None, 0)),
    )
    .unwrap();
    let frame: Option<BackendFrame> = read_line(&mut agent.reader).unwrap();
    assert!(frame.is_none(), "an exited app gets no reply: {frame:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "nothing was left behind, so nothing is waited for: {:?}",
        started.elapsed()
    );
    let status = agent
        .wait_exit(Duration::from_secs(10))
        .expect("the agent ends");
    assert!(!status.success(), "{status}");
}

#[test]
fn resize_changes_the_window_geometry_and_the_capture() {
    if !session_tests() {
        return;
    }
    let (mut agent, hello) = Agent::start(PROTOCOL_VERSION);
    assert!(
        matches!(hello, Outcome::Ok(Reply::Hello { .. })),
        "{hello:?}"
    );
    let too_big = agent.err(Request::Resize {
        width: 1400.,
        height: 600.,
        settle_ms: 0,
    });
    assert_eq!(too_big.code, ErrorCode::InvalidArgument);
    assert!(too_big.message.contains("1280x800"), "{}", too_big.message);
    let Reply::Resize { window, settled } = agent.ok(Request::Resize {
        width: 960.,
        height: 600.,
        settle_ms: 2000,
    }) else {
        panic!("not a resize")
    };
    assert_eq!((window.w, window.h, window.scale), (960., 600., 1.5));
    assert!(settled);
    let shot = agent.dir.path().join("resized.png");
    let Reply::Screenshot { w, h, .. } = agent.ok(Request::Screenshot {
        target: None,
        path: shot,
    }) else {
        panic!("not a screenshot")
    };
    assert_eq!((w, h), (1440, 900));
    let Reply::Tree { nodes, .. } = agent.ok(tree(None, None, 0)) else {
        panic!("not a tree")
    };
    let viewport = nodes
        .iter()
        .find(|n| n.id.as_deref() == Some("viewport"))
        .expect("the viewport readout");
    assert_eq!(viewport.name.as_deref(), Some("Viewport 960x600"));
    assert_eq!(agent.ok(Request::Close), Reply::Closed);
}
