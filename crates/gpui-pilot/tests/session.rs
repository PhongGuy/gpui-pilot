//! The session vocabulary end to end: rmcp client, server, sandbox, KWin and the windowed demo.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Pilot, alive, png};
use gpui_pilot_session::sandbox::{Network, preflight};
use serde_json::{Value, json};

/// True when the session suite should run; `network` names the tools this test needs.
fn session_tests(network: Network) -> bool {
    if std::env::var_os("GPUI_PILOT_SESSION_TESTS").as_deref() != Some("1".as_ref()) {
        eprintln!(
            "skipped: set GPUI_PILOT_SESSION_TESTS=1 to run session tests (they need bwrap, kwin_wayland and a GPU)"
        );
        return false;
    }
    if let Err(error) = preflight(network) {
        panic!("GPUI_PILOT_SESSION_TESTS=1 but the session cannot run: {error}");
    }
    true
}

fn windowed() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_gpui-pilot")).with_file_name("pilot-demo-windowed")
}

/// The demo built with the `timing` profile (dev with opt-level 3), beside `debug`.
fn timing_windowed() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_gpui-pilot"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("timing/pilot-demo-windowed")
}

/// Fails up front, naming the one command that builds them, when a timing-profile binary is
/// missing. A test does not build them itself: an optimised GPUI build takes minutes, so it
/// would hold a test thread (and the session tests' 5 s start timeout) hostage.
fn require_timing_bins(bins: &[&Path]) {
    if let Some(bin) = bins.iter().find(|bin| !bin.exists()) {
        panic!(
            "{} is missing; run `cargo build -p gpui-pilot -p pilot-demo --bins --profile timing` \
             first (honours CARGO_TARGET_DIR)",
            bin.display()
        );
    }
}

fn config() -> String {
    format!("[session]\nbinary = \"{}\"\n", windowed().display())
}

fn launch(network: &str) -> Value {
    json!({
        "mode": "session", "width": 1280, "height": 800, "scale": 1.5, "build": false,
        "network": network, "env": {"PILOT_DEMO_GREETING": "hello"}
    })
}

/// Every live descendant of `root`, found through the parent pids in `/proc/*/stat`.
fn descendants(root: u32) -> Vec<u32> {
    let mut parent = HashMap::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(after) = stat.rsplit(") ").next() else {
            continue;
        };
        if let Some(ppid) = after.split(' ').nth(1).and_then(|p| p.parse::<u32>().ok()) {
            parent.insert(pid, ppid);
        }
    }
    let mut found = vec![root];
    let mut at = 0;
    while at < found.len() {
        let current = found[at];
        found.extend(
            parent
                .iter()
                .filter(|(_, p)| **p == current)
                .map(|(c, _)| *c),
        );
        at += 1;
    }
    found.remove(0);
    found
}

/// The launch directory (the sandbox's `dir`): the parent of the reported `data_dir`.
fn launch_dir(launched: &Value) -> PathBuf {
    let dir = Path::new(launched["data_dir"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    assert!(dir.is_dir(), "{}", dir.display());
    dir
}

async fn assert_no_leaks(pilot: &Pilot, inside: Vec<u32>, dir: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while inside.iter().any(|p| alive(*p as u64)) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let left: Vec<_> = inside.iter().filter(|p| alive(**p as u64)).collect();
    assert!(left.is_empty(), "processes outlived close: {left:?}");
    assert!(
        !dir.exists(),
        "close left the sandbox directory {}",
        dir.display()
    );
    pilot.assert_bus_untouched();
}

#[tokio::test(flavor = "multi_thread")]
async fn drives_the_windowed_demo_in_a_private_session() {
    if !session_tests(Network::None) {
        return;
    }
    assert!(
        windowed().exists(),
        "run `cargo build -p pilot-demo --bins` first"
    );
    let pilot = Pilot::start(Some(&config())).await;
    let launched = pilot.ok("launch", launch("none")).await;
    let dir = launch_dir(&launched);
    assert_eq!(launched["mode"], json!("session"));
    assert_eq!(
        (
            launched["window"]["w"].clone(),
            launched["window"]["scale"].clone()
        ),
        (json!(1280.0), json!(1.5))
    );
    assert_eq!(launched["settled"], json!(true));
    assert_eq!(
        launched["refresh_hz"],
        json!(143.838),
        "default 144 Hz at 1920x1200 device pixels"
    );

    let tree = pilot
        .ok("tree", json!({"wait_for": {"id": ["count"]}}))
        .await;
    let nodes = tree["nodes"].as_array().unwrap();
    assert!(
        nodes
            .iter()
            .any(|n| n["role"] == json!("Button") && n["name"] == json!("hello")),
        "launch.env reached the app: {tree}"
    );
    let clicked = pilot
        .ok("click", json!({"target": {"id": ["count"]}}))
        .await;
    assert_eq!(
        (clicked["clicked"]["id"].clone(), clicked["settled"].clone()),
        (json!("count"), json!(true))
    );
    let before = png(&pilot.result("screenshot", json!({})).await);
    pilot
        .ok(
            "click",
            json!({"target": {"role": "Button", "name": "Count"}}),
        )
        .await;
    let after = png(&pilot.result("screenshot", json!({})).await);
    assert_ne!(before, after, "the counter changed the pixels");

    pilot
        .ok("type", json!({"text": "Hié", "target": {"id": ["name"]}}))
        .await;
    pilot.ok("type", json!({"text": "漢"})).await;
    pilot.ok("key", json!({"keys": "backspace"})).await;
    pilot
        .ok("scroll", json!({"target": {"id": ["name"]}, "dy": 40}))
        .await;
    let hovered = pilot
        .ok("hover", json!({"target": {"id": ["count"]}}))
        .await;
    assert_eq!(hovered["hovered"]["id"], json!("count"));
    let waited = pilot.ok("wait", json!({"ms": 100})).await;
    assert!(waited["waited_ms"].as_u64().unwrap() >= 100);

    let saved = pilot.root().join("saved.png");
    let shot = pilot.ok("screenshot", json!({"save_to": saved})).await;
    assert_eq!(shot["w"], json!(1920));
    assert!(std::fs::read(&saved).unwrap().starts_with(b"\x89PNG"));

    let inside = descendants(pilot.server_pid);
    assert!(
        inside.len() >= 5,
        "bwrap, the bus, KWin, the agent and the app: {inside:?}"
    );
    let closed = pilot.ok("close", json!({})).await;
    assert!(closed["log_tail"].is_string());
    assert_no_leaks(&pilot, inside, &dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pasta_network_mode_drives_and_tears_down_too() {
    if !session_tests(Network::Pasta) {
        return;
    }
    let pilot = Pilot::start(Some(&config())).await;
    let dir = launch_dir(&pilot.ok("launch", launch("pasta")).await);
    let clicked = pilot
        .ok("click", json!({"target": {"id": ["count"]}}))
        .await;
    assert_eq!(clicked["settled"], json!(true));
    let inside = descendants(pilot.server_pid);
    assert!(
        inside.iter().any(|p| {
            std::fs::read_to_string(format!("/proc/{p}/comm")).is_ok_and(|c| c.starts_with("passt"))
        }),
        "pasta (comm passt.*) wraps the sandbox in the server's process tree: {inside:?}"
    );
    pilot.ok("close", json!({})).await;
    assert_no_leaks(&pilot, inside, &dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pick_file_stages_host_files_and_the_clipboard_reaches_the_app() {
    if !session_tests(Network::None) {
        return;
    }
    let pilot = Pilot::start(Some(&config())).await;
    let dir = launch_dir(&pilot.ok("launch", launch("none")).await);
    std::fs::write(pilot.root().join("fixture.txt"), "from the host").unwrap();
    let open = json!({"target": {"role": "Button", "name": "Open"}});

    pilot.ok("click", open.clone()).await;
    let opened = pilot
        .ok("pick_file", json!({"paths": ["fixture.txt"]}))
        .await;
    // The pick dirs live in a `gp-` sibling of the launch dir, found from the staged path.
    let staged = PathBuf::from(opened["paths"][0].as_str().unwrap());
    let pick = staged.ancestors().nth(3).unwrap().to_path_buf();
    assert!(
        pick != dir && pick.parent() == dir.parent(),
        "{}",
        pick.display()
    );
    assert!(
        pick.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("gp-")
    );
    assert_eq!(staged, pick.join("in/1/fixture.txt"));
    assert_eq!(
        (opened["answered"].clone(), opened["paths"].clone()),
        (json!("open"), json!([staged]))
    );
    assert_eq!(std::fs::read_to_string(&staged).unwrap(), "from the host");

    pilot
        .ok(
            "click",
            json!({"target": {"role": "Button", "name": "Save"}}),
        )
        .await;
    let over = pilot
        .err("pick_file", json!({"paths": ["fixture.txt"]}))
        .await;
    assert_eq!(
        over["code"],
        json!("InvalidArgument"),
        "overwriting an existing file is unsupported"
    );
    let saved = pilot
        .ok("pick_file", json!({"paths": ["report.txt"]}))
        .await;
    let redirected = pick.join("out/3/report.txt");
    assert_eq!(
        (saved["answered"].clone(), saved["paths"].clone()),
        (json!("save"), json!([redirected]))
    );

    pilot.ok("click", open).await;
    let cancelled = pilot.ok("pick_file", json!({"cancel": true})).await;
    assert_eq!(cancelled["answered"], json!("cancelled"));
    let none = pilot
        .err("pick_file", json!({"cancel": true, "timeout_ms": 300}))
        .await;
    assert_eq!(none["code"], json!("Timeout"));

    let clip = pilot.ok("clipboard", json!({"set": "héllo ✓"})).await;
    assert_eq!(clip["text"], json!("héllo ✓"));
    pilot
        .ok("type", json!({"text": "a漢", "target": {"id": ["name"]}}))
        .await;
    pilot.ok("key", json!({"keys": "ctrl-a ctrl-c"})).await;
    assert_eq!(pilot.ok("clipboard", json!({})).await["text"], json!("a漢"));
    pilot.ok("close", json!({})).await;
    assert!(
        !pick.exists(),
        "close left the pick directory {}",
        pick.display()
    );
    pilot.assert_bus_untouched();
}

#[tokio::test(flavor = "multi_thread")]
async fn keep_data_restarts_the_session_on_the_same_home() {
    if !session_tests(Network::None) {
        return;
    }
    let pilot = Pilot::start(Some(&config())).await;
    let mut args = launch("none");
    args["env"]["PILOT_DEMO_PERSIST"] = json!("1");
    let first = pilot.ok("launch", args.clone()).await;
    assert_eq!(first["reused"], json!(false));
    let dir = launch_dir(&first);
    let clicks = dir.join("home/clicks");
    pilot
        .ok("click", json!({"target": {"id": ["count"]}}))
        .await;
    pilot
        .ok("click", json!({"target": {"id": ["count"]}}))
        .await;
    assert_eq!(std::fs::read_to_string(&clicks).unwrap(), "2");
    let inside = descendants(pilot.server_pid);
    let closed = pilot.ok("close", json!({"keep_data": true})).await;
    assert_eq!(closed["data_dir"], first["data_dir"]);
    let deadline = Instant::now() + Duration::from_secs(3);
    while inside.iter().any(|p| alive(*p as u64)) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    args["keep_data"] = json!(true);
    let second = pilot.ok("launch", args).await;
    assert_eq!(second["reused"], json!(true));
    assert_eq!(launch_dir(&second), dir);
    pilot
        .ok("tree", json!({"wait_for": {"id": ["count"]}}))
        .await;
    pilot
        .ok("click", json!({"target": {"id": ["count"]}}))
        .await;
    assert_eq!(
        std::fs::read_to_string(&clicks).unwrap(),
        "3",
        "the app restored its count"
    );
    let inside = descendants(pilot.server_pid);
    pilot.ok("close", json!({})).await;
    assert_no_leaks(&pilot, inside, &dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_scroll_and_window_close_reach_the_session_app() {
    if !session_tests(Network::None) {
        return;
    }
    let pilot = Pilot::start(Some(&config())).await;
    let mut args = launch("none");
    args["env"]["PILOT_DEMO_GUARD_CLOSE"] = json!("1");
    let launched = pilot.ok("launch", args).await;
    let dir = launch_dir(&launched);
    pilot
        .ok("tree", json!({"wait_for": {"id": ["name"]}}))
        .await;
    pilot
        .ok(
            "scroll",
            json!({"target": {"id": ["name"]}, "dy": 15, "modifiers": ["ctrl"]}),
        )
        .await;
    pilot
        .ok(
            "tree",
            json!({"wait_for": {"role": "Button", "name": "Zoom 1"}}),
        )
        .await;
    let kept = pilot.ok("close", json!({"mode": "window"})).await;
    assert_eq!(kept["closed"], json!(false), "{kept}");
    pilot
        .ok("tree", json!({"wait_for": {"id": ["count"]}}))
        .await;
    let inside = descendants(pilot.server_pid);
    pilot.ok("close", json!({})).await;
    assert_no_leaks(&pilot, inside, &dir).await;

    let launched = pilot.ok("launch", launch("none")).await;
    let dir = launch_dir(&launched);
    pilot
        .ok("tree", json!({"wait_for": {"id": ["count"]}}))
        .await;
    let inside = descendants(pilot.server_pid);
    let closed = pilot.ok("close", json!({"mode": "window"})).await;
    assert_eq!(closed["closed"], json!(true), "{closed}");
    assert_no_leaks(&pilot, inside, &dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pasta_host_ports_reach_a_local_server_and_none_does_not() {
    if !session_tests(Network::Pasta) {
        return;
    }
    let button = |name: &str| json!({"role": "Button", "name": name});
    for (network, expected) in [("pasta", "fetched: hello"), ("none", "fetch failed")] {
        let port = common::serve_once("hello");
        // The config default supplies pasta; the launch names only `none` explicitly.
        let pilot = Pilot::start(Some(&format!("{}network = \"pasta\"\n", config()))).await;
        let mut args = launch(network);
        if network == "pasta" {
            args.as_object_mut().unwrap().remove("network");
            args["host_ports"] = json!([port]);
        }
        args["args"] = json!(["--fetch", format!("http://127.0.0.1:{port}/")]);
        let launched = pilot.ok("launch", args).await;
        assert_eq!(launched["network"], json!(network));
        let dir = launch_dir(&launched);
        pilot.ok("click", json!({"target": button("Fetch")})).await;
        pilot
            .ok(
                "tree",
                json!({"wait_for": button(expected), "timeout_ms": 10000}),
            )
            .await;
        let inside = descendants(pilot.server_pid);
        pilot.ok("close", json!({})).await;
        assert_no_leaks(&pilot, inside, &dir).await;
        pilot.assert_bus_untouched();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn launch_arg_paths_are_bound_read_write_into_the_sandbox() {
    if !session_tests(Network::None) {
        return;
    }
    // Outside /tmp, like a collection under ~/.cache.
    let shared = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::fs::write(shared.path().join("hello.txt"), "hi").unwrap();
    let pilot = Pilot::start(Some(&config())).await;
    let mut args = launch("none");
    args["args"] = json!(["--dir", shared.path()]);
    let launched = pilot.ok("launch", args).await;
    let dir = launch_dir(&launched);
    let button = |name: &str| json!({"role": "Button", "name": name});
    pilot
        .ok("tree", json!({"wait_for": button("listing: hello.txt")}))
        .await;
    pilot.ok("click", json!({"target": button("Touch")})).await;
    let touched = shared.path().join("touched");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !touched.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(std::fs::read_to_string(&touched).unwrap(), "pilot-demo");
    let inside = descendants(pilot.server_pid);
    pilot.ok("close", json!({})).await;
    assert_no_leaks(&pilot, inside, &dir).await;

    // A path that reaches the desktop session is refused before anything starts.
    let pilot = Pilot::start(Some(&config())).await;
    let mut args = launch("none");
    args["args"] = json!(["--dir", "/run"]);
    assert_eq!(
        pilot.err("launch", args).await["code"],
        json!("InvalidArgument")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_output_runs_at_the_requested_rate_or_refuses() {
    if !session_tests(Network::None) {
        return;
    }
    assert!(
        windowed().exists(),
        "run `cargo build -p pilot-demo --bins` first"
    );
    let pilot = Pilot::start(Some(&config())).await;
    let at = |hz: u32, width: u32| {
        json!({"mode": "session", "width": width, "height": 800, "scale": 1.0, "build": false,
               "replace": true, "refresh_hz": hz})
    };
    let odd = pilot.err("launch", at(144, 1100)).await;
    assert_eq!(odd["code"], json!("InvalidArgument"), "{odd}");
    let fast = pilot.ok("launch", at(144, 1280)).await;
    assert_eq!(fast["refresh_hz"], json!(143.832));
    let slow = pilot.ok("launch", at(60, 1280)).await;
    assert_eq!(slow["refresh_hz"], json!(60.0));
    let fastest = pilot.ok("launch", at(240, 1280)).await;
    assert_eq!(fastest["refresh_hz"], json!(239.776));
    pilot.ok("close", json!({})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn session_frames_follow_the_output_under_fast_notches() {
    if !session_tests(Network::None) {
        return;
    }
    let demo = timing_windowed();
    require_timing_bins(&[&demo]);
    let pilot = Pilot::start(Some(&format!(
        "[session]\nbinary = \"{}\"\n",
        demo.display()
    )))
    .await;
    let launched = pilot
        .ok(
            "launch",
            json!({"mode": "session", "width": 1280, "height": 800, "scale": 1.0,
                   "build": false, "args": ["--list"]}),
        )
        .await;
    assert_eq!(launched["refresh_hz"], json!(143.832));
    assert_eq!(
        pilot.ok("frames", json!({"action": "start"})).await,
        json!({"refresh_mhz": 143_832})
    );
    // Notches 5 ms apart outpace the output, so every vblank has a frame due.
    pilot
        .ok(
            "scroll",
            json!({"target": {"x": 640, "y": 380}, "notches": 100, "notch_ms": 5, "settle_ms": 0}),
        )
        .await;
    pilot.ok("wait", json!({"ms": 100})).await;
    let report = pilot.ok("frames", json!({"action": "stop"})).await;
    let period = 1e6 / 143_832.0;
    let load = common::load::report();
    let median = report["intervals"]["median_ms"]
        .as_f64()
        .expect("intervals");
    assert!(
        (median - period).abs() <= 0.5,
        "median {median} ms against a {period} ms period: {report}; {load}"
    );
    assert!(report["frames"].as_u64().unwrap() >= 50, "{report}; {load}");
    // The last notches' frames may race the stop, so 95 of 100 suffice.
    assert!(
        report["latency"]["count"].as_u64().unwrap() >= 95,
        "{report}; {load}"
    );
    assert!(
        report.get("work").is_none() && report.get("build").is_none(),
        "{report}"
    );
    assert_eq!(report["warnings"], json!([]), "{report}");
    pilot.ok("close", json!({})).await;
}

/// The server, and so its agent, built with the `timing` profile, whose captures are as fast as
/// a release build's; beside `debug`.
fn timing_server() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_gpui-pilot"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("timing/gpui-pilot")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_screenshot_after_a_click_shows_the_apps_last_frame() {
    if !session_tests(Network::None) {
        return;
    }
    let (server, demo) = (timing_server(), timing_windowed());
    require_timing_bins(&[&server, &demo]);
    let config = format!("[session]\nbinary = \"{}\"\n", demo.display());
    let pilot = Pilot::start_with(&server, Some(&config)).await;
    pilot
        .ok(
            "launch",
            json!({"mode": "session", "width": 1280, "height": 800, "scale": 1.0,
                   "build": false, "args": ["--late-paint-ms", "150"]}),
        )
        .await;
    // Each click paints a pending label at once and the count 150 ms later: a gap that two
    // captures 50 ms apart fit inside, but the 160 ms quiet window does not.
    for n in 1..=5 {
        let clicked = pilot
            .ok("click", json!({"target": {"id": ["repaint"]}}))
            .await;
        assert_eq!(clicked["settled"], json!(true), "click {n}");
        let shot = png(&pilot.result("screenshot", json!({})).await);
        pilot.ok("wait", json!({"ms": 500})).await;
        let truth = png(&pilot.result("screenshot", json!({})).await);
        assert!(
            shot == truth,
            "click {n}: the screenshot showed a frame before the app's last; {}",
            common::load::report()
        );
    }
    pilot.ok("close", json!({})).await;
}
