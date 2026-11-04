use std::io::{BufReader, ErrorKind};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use gpui_pilot_protocol::{
    BackendFrame, ClientFrame, ErrorCode, LaunchParams, MouseButton, Outcome, PROTOCOL_VERSION,
    Reply, Request, Segment, Target, read_line, write_line,
};

struct Host {
    child: Child,
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    /// Stand-ins for the system and session buses: any D-Bus connect lands here.
    buses: [UnixListener; 2],
}

fn listen(path: &Path) -> UnixListener {
    let listener = UnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

fn spawn_host(dir: &Path) -> Host {
    let socket = dir.join("pilot.sock");
    let listener = listen(&socket);
    let buses = [
        listen(&dir.join("system_bus")),
        listen(&dir.join("session_bus")),
    ];
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pilot-demo"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("XDG_RUNTIME_DIR", dir)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env(
            "DBUS_SYSTEM_BUS_ADDRESS",
            format!("unix:path={}", dir.join("system_bus").display()),
        )
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", dir.join("session_bus").display()),
        )
        .env("GPUI_PILOT_SOCKET", &socket)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(_) if Instant::now() < deadline => {
                assert!(child.try_wait().unwrap().is_none(), "the host exited early");
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("the host never connected: {error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let reader = BufReader::new(stream.try_clone().unwrap());
    Host {
        child,
        stream,
        reader,
        buses,
    }
}

fn call(host: &mut Host, id: u64, request: Request) -> Outcome {
    write_line(&mut host.stream, &ClientFrame::new(id, request)).unwrap();
    let frame: BackendFrame = read_line(&mut host.reader)
        .unwrap()
        .expect("a reply before end of stream");
    assert_eq!(frame.id, id);
    frame.outcome
}

fn hello(version: u32, dir: &Path) -> Request {
    let launch = LaunchParams::new(640.0, 400.0, 1.0, dir.join("data"), Vec::new());
    Request::Hello { version, launch }
}

fn click_count() -> Request {
    Request::Click {
        target: Target::Id {
            id: vec![Segment::Str("count".into())],
        },
        button: MouseButton::Left,
        modifiers: Vec::new(),
        count: 1,
        timeout_ms: 1000,
        settle_ms: 2000,
    }
}

fn close_window() -> Request {
    Request::CloseWindow {
        timeout_ms: 1000,
        settle_ms: 2000,
    }
}

fn assert_no_bus_connection(host: &Host) {
    for (bus, name) in host.buses.iter().zip(["system", "session"]) {
        match bus.accept() {
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Ok(_) => panic!("the host connected to the {name} bus"),
            Err(error) => panic!("checking the {name} bus: {error}"),
        }
    }
}

fn exit_within(host: &mut Host, limit: Duration) -> ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = host.child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            host.child.kill().ok();
            panic!("the host did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn serves_hello_click_screenshot_and_close() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = spawn_host(dir.path());
    let Outcome::Ok(Reply::Hello {
        version, window, ..
    }) = call(&mut host, 1, hello(PROTOCOL_VERSION, dir.path()))
    else {
        panic!("hello failed");
    };
    assert_eq!(
        (version, window.w, window.scale),
        (PROTOCOL_VERSION, 640.0, 1.0)
    );
    assert!(matches!(
        call(&mut host, 2, click_count()),
        Outcome::Ok(Reply::Click { settled: true, .. })
    ));
    let shot = dir.path().join("shot.png");
    let screenshot = Request::Screenshot {
        target: None,
        path: shot.clone(),
    };
    assert!(matches!(
        call(&mut host, 3, screenshot),
        Outcome::Ok(Reply::Screenshot { w: 640, h: 400, .. })
    ));
    assert!(std::fs::read(&shot).unwrap().starts_with(b"\x89PNG"));
    assert_no_bus_connection(&host);
    assert!(matches!(
        call(&mut host, 4, Request::Close),
        Outcome::Ok(Reply::Closed)
    ));
    assert!(exit_within(&mut host, Duration::from_secs(10)).success());
}

#[test]
fn a_protocol_mismatch_is_reported_and_the_host_exits() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = spawn_host(dir.path());
    let outcome = call(&mut host, 1, hello(PROTOCOL_VERSION + 1, dir.path()));
    let Outcome::Err(error) = outcome else {
        panic!("expected a mismatch: {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::ProtocolMismatch);
    assert!(!exit_within(&mut host, Duration::from_secs(10)).success());
}

#[test]
fn end_of_stream_ends_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = spawn_host(dir.path());
    assert!(matches!(
        call(&mut host, 1, hello(PROTOCOL_VERSION, dir.path())),
        Outcome::Ok(Reply::Hello { .. })
    ));
    host.stream.shutdown(std::net::Shutdown::Both).unwrap();
    exit_within(&mut host, Duration::from_secs(10));
}

#[test]
fn hello_waits_for_the_app_to_settle_after_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = spawn_host(dir.path());
    let mut launch = LaunchParams::new(
        640.0,
        400.0,
        1.0,
        dir.path().join("data"),
        vec!["--boot".into()],
    );
    launch.settle_ms = 3000;
    let hello = Request::Hello {
        version: PROTOCOL_VERSION,
        launch,
    };
    let Outcome::Ok(Reply::Hello { settled, .. }) = call(&mut host, 1, hello) else {
        panic!("hello failed");
    };
    assert!(settled);
    let tree = Request::Tree {
        within: None,
        wait_for: None,
        timeout_ms: 0,
        settle_ms: 0,
    };
    let Outcome::Ok(Reply::Tree { nodes, .. }) = call(&mut host, 2, tree) else {
        panic!("tree failed");
    };
    let booted = Segment::Str("booted".into());
    assert!(
        nodes.iter().any(|n| n.path.last() == Some(&booted)),
        "the boot timer finished before hello"
    );
    assert!(matches!(
        call(&mut host, 3, Request::Close),
        Outcome::Ok(Reply::Closed)
    ));
    assert!(exit_within(&mut host, Duration::from_secs(10)).success());
}

#[test]
fn close_window_answers_then_the_host_exits() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = spawn_host(dir.path());
    assert!(matches!(
        call(&mut host, 1, hello(PROTOCOL_VERSION, dir.path())),
        Outcome::Ok(Reply::Hello { .. })
    ));
    assert_eq!(
        call(&mut host, 2, close_window()),
        Outcome::Ok(Reply::CloseWindow {
            closed: true,
            settled: true
        })
    );
    let status = exit_within(&mut host, Duration::from_secs(10));
    assert!(status.success(), "{status}");
    assert_no_bus_connection(&host);
}

#[test]
fn a_guarded_close_waits_and_discard_is_answered_before_the_exit() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = spawn_host(dir.path());
    let launch = LaunchParams::new(
        640.0,
        400.0,
        1.0,
        dir.path().join("data"),
        vec!["--guard-close".into()],
    );
    let hello = Request::Hello {
        version: PROTOCOL_VERSION,
        launch,
    };
    assert!(matches!(
        call(&mut host, 1, hello),
        Outcome::Ok(Reply::Hello { .. })
    ));
    assert!(matches!(
        call(&mut host, 2, close_window()),
        Outcome::Ok(Reply::CloseWindow { closed: false, .. })
    ));
    let discard = Request::Dialog {
        button: Some("Discard".into()),
        timeout_ms: 1000,
        settle_ms: 2000,
    };
    let answered = call(&mut host, 3, discard);
    assert!(
        matches!(&answered, Outcome::Ok(Reply::Dialog { answered: Some(label), .. }) if label == "Discard"),
        "{answered:?}"
    );
    let status = exit_within(&mut host, Duration::from_secs(10));
    assert!(
        status.success(),
        "the app quit with its last window: {status}"
    );
}

#[test]
fn hello_reports_the_launch_rate_and_refuses_an_unmeasured_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = spawn_host(dir.path());
    let Outcome::Ok(Reply::Hello { refresh_mhz, .. }) =
        call(&mut host, 1, hello(PROTOCOL_VERSION, dir.path()))
    else {
        panic!("hello failed");
    };
    assert_eq!(refresh_mhz, 144_000);
    assert!(matches!(
        call(&mut host, 2, Request::Close),
        Outcome::Ok(Reply::Closed)
    ));
    assert!(exit_within(&mut host, Duration::from_secs(10)).success());

    let dir = tempfile::tempdir().unwrap();
    let mut host = spawn_host(dir.path());
    let mut launch = LaunchParams::new(640.0, 400.0, 1.0, dir.path().join("data"), Vec::new());
    launch.refresh_hz = 75;
    let outcome = call(
        &mut host,
        1,
        Request::Hello {
            version: PROTOCOL_VERSION,
            launch,
        },
    );
    let Outcome::Err(error) = outcome else {
        panic!("75 Hz was accepted: {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(error.message.contains("refresh_hz"), "{}", error.message);
    assert!(!exit_within(&mut host, Duration::from_secs(10)).success());
}

#[test]
fn a_frame_the_host_cannot_read_gets_an_error_reply_and_the_session_goes_on() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let mut host = spawn_host(dir.path());
    assert!(matches!(
        call(&mut host, 1, hello(PROTOCOL_VERSION, dir.path())),
        Outcome::Ok(Reply::Hello { .. })
    ));
    // A request from a future protocol: a field this host does not know.
    let future = r#"{"id":2,"request":{"op":"click","target":{"id":["count"]},"button":"left","count":1,"from_the_future":[],"timeout_ms":1000,"settle_ms":100}}"#;
    writeln!(host.stream, "{future}").unwrap();
    let reply: BackendFrame = read_line(&mut host.reader).unwrap().unwrap();
    assert_eq!(reply.id, 2);
    let Outcome::Err(error) = reply.outcome else {
        panic!("expected an error: {:?}", reply.outcome);
    };
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(
        error.message.contains("from_the_future"),
        "{}",
        error.message
    );
    // The host still serves.
    let clicked = call(&mut host, 3, click_count());
    assert!(
        matches!(clicked, Outcome::Ok(Reply::Click { .. })),
        "{clicked:?}"
    );
    assert_eq!(
        call(&mut host, 4, Request::Close),
        Outcome::Ok(Reply::Closed)
    );
    assert_eq!(
        exit_within(&mut host, Duration::from_secs(10)).code(),
        Some(0)
    );
}
