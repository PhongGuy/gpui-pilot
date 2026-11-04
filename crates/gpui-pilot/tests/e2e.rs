//! The whole headless vocabulary against pilot-demo, through rmcp's client over stdio.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;

use common::{Pilot, alive, kill, pilot_demo, png, server_command};
use gpui_pilot_protocol::PROTOCOL_VERSION;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

fn config() -> String {
    format!("[headless]\nbinary = \"{}\"\n", pilot_demo().display())
}

fn launch_args() -> Value {
    json!({"mode": "headless", "width": 640, "height": 400, "build": false})
}

fn id(name: &str) -> Value {
    json!({"id": [name]})
}

fn ends_with(path: &Value, name: &str) -> bool {
    path.as_array().and_then(|p| p.last()) == Some(&json!(name))
}

#[tokio::test(flavor = "multi_thread")]
async fn drives_the_demo_through_the_whole_vocabulary() {
    let pilot = Pilot::start(Some(&config())).await;
    let launched = pilot.ok("launch", launch_args()).await;
    assert_eq!(launched["mode"], json!("headless"));
    assert_eq!(launched["network"], json!("host"));
    assert_eq!(
        launched["window"],
        json!({"w": 640.0, "h": 400.0, "scale": 1.0})
    );
    let data_dir = PathBuf::from(launched["data_dir"].as_str().unwrap());
    assert!(
        data_dir.is_dir() && data_dir.starts_with(pilot.temp.path()),
        "{}",
        data_dir.display()
    );

    let before = png(&pilot.result("screenshot", json!({})).await);
    assert!(before.starts_with(b"\x89PNG"));
    let unsaved = data_dir.parent().unwrap().join("shot-1.png");
    let mode = std::fs::metadata(&unsaved).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o777,
        0o600,
        "screenshots in the launch directory are private"
    );
    let clicked = pilot.ok("click", json!({"target": id("count")})).await;
    assert!(ends_with(&clicked["clicked"]["path"], "count"), "{clicked}");
    assert_eq!(clicked["settled"], json!(true));
    let shot = pilot
        .result("screenshot", json!({"save_to": "after.png"}))
        .await;
    let after = png(&shot);
    assert_ne!(before, after, "the click changed the counter text");
    let saved = pilot.root().join("after.png");
    assert_eq!(shot.structured_content.unwrap()["saved_to"], json!(saved));
    assert_eq!(std::fs::read(&saved).unwrap(), after);
    // Hover and press styling alone change the first shot; a second click under the same
    // hover state changes only the `clicks: N` label.
    pilot.ok("click", json!({"target": id("count")})).await;
    let again = png(&pilot.result("screenshot", json!({})).await);
    assert_ne!(after, again, "the second click changed the counter text");
    let crop = pilot.ok("screenshot", json!({"target": id("count")})).await;
    assert!(crop["w"].as_u64().unwrap() < 640, "{crop}");

    let tree = pilot.ok("tree", json!({"wait_for": id("paste")})).await;
    for name in ["count", "name", "open", "save", "paste"] {
        assert!(
            tree["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| ends_with(&n["path"], name)),
            "{name}"
        );
    }
    let typed = pilot
        .ok("type", json!({"text": "abc", "target": id("name")}))
        .await;
    assert_eq!(typed, json!({"settled": true}));
    assert_eq!(
        pilot.ok("key", json!({"keys": "backspace"})).await,
        json!({"settled": true})
    );
    let scrolled = pilot
        .ok("scroll", json!({"target": id("name"), "dy": 40}))
        .await;
    assert_eq!(scrolled, json!({"settled": true}));

    // A cancel with no dialog open is queued and taken by the next one; a pending one is
    // answered at once.
    let queued = pilot.ok("pick_file", json!({"cancel": true})).await;
    assert_eq!(queued["answered"], json!("queued"));
    pilot.ok("click", json!({"target": id("open")})).await;
    pilot.ok("click", json!({"target": id("save")})).await;
    let answered = pilot.ok("pick_file", json!({"paths": ["out.txt"]})).await;
    assert_eq!(answered["answered"], json!("save"));
    assert_eq!(answered["prompt"]["kind"], json!("save"));
    assert_eq!(answered["prompt"]["suggested_name"], json!("demo.txt"));

    assert_eq!(
        pilot.ok("clipboard", json!({"set": "pasted"})).await["text"],
        json!("pasted")
    );
    assert_eq!(
        pilot.ok("clipboard", json!({})).await["text"],
        json!("pasted")
    );

    let closed = pilot.ok("close", json!({})).await;
    assert_eq!(closed["exit_code"], json!(0));
    assert!(closed["log_tail"].is_string());
    assert!(!data_dir.exists(), "close removes the launch directory");
    pilot.assert_bus_untouched();
}

#[tokio::test(flavor = "multi_thread")]
async fn host_errors_replacement_and_a_crash_map_to_codes() {
    let pilot = Pilot::start(Some(&config())).await;
    let first = pilot.ok("launch", launch_args()).await["pid"]
        .as_u64()
        .unwrap();
    assert_eq!(
        pilot.err("launch", launch_args()).await["code"],
        json!("AlreadyRunning")
    );
    let missing = pilot
        .err(
            "click",
            json!({"target": id("cont"), "timeout_ms": 100, "settle_ms": 100}),
        )
        .await;
    assert_eq!(missing["code"], json!("ElementNotFound"));
    assert!(
        missing["nearest"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| ends_with(p, "count")),
        "{missing}"
    );
    assert_eq!(
        pilot.err("key", json!({"keys": "ctrl-a-b"})).await["code"],
        json!("InvalidKeystroke")
    );
    let waited = pilot
        .err("tree", json!({"wait_for": id("nope"), "timeout_ms": 100}))
        .await;
    assert_eq!(waited["code"], json!("Timeout"));

    let mut replace = launch_args();
    replace["replace"] = json!(true);
    let second = pilot.ok("launch", replace).await["pid"].as_u64().unwrap();
    assert_ne!(first, second);
    assert!(!alive(first), "replace stops the first host");

    kill(second);
    let crashed = pilot.err("click", json!({"target": id("count")})).await;
    assert_eq!(crashed["code"], json!("AppCrashed"));
    assert!(crashed["log_tail"].is_string());
    assert_eq!(
        pilot.err("click", json!({"target": id("count")})).await["code"],
        json!("NotRunning")
    );
}

#[tokio::test]
async fn build_and_start_failures_carry_the_log_tail() {
    let config = "[headless]\nbuild = [\"sh\", \"-c\", \"echo compiling; echo 'error[E0425]' >&2; exit 101\"]\nbinary = \"/usr/bin/true\"\n";
    let pilot = Pilot::start(Some(config)).await;
    let build = pilot.err("launch", json!({"mode": "headless"})).await;
    assert_eq!(build["code"], json!("BuildFailed"));
    assert_eq!(build["log_tail"], json!("compiling\nerror[E0425]"));
    let start = pilot
        .err("launch", json!({"mode": "headless", "build": false}))
        .await;
    assert_eq!(start["code"], json!("LaunchFailed"));
    assert!(
        start["message"]
            .as_str()
            .unwrap()
            .contains("exited before connecting"),
        "{start}"
    );
    let release = Pilot::start(Some("[headless]\nbinary = \"target/release/app\"\n")).await;
    assert_eq!(
        release.err("launch", json!({"mode": "headless"})).await["code"],
        json!("InvalidConfig")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_killed_server_takes_the_host_with_it() {
    let pilot = Pilot::start(Some(&config())).await;
    let host = pilot.ok("launch", launch_args()).await["pid"]
        .as_u64()
        .unwrap();
    assert!(alive(host), "the host runs before the server is killed");
    kill(u64::from(pilot.server_pid));
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(host) {
        assert!(Instant::now() < deadline, "the host outlived the server");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_the_server_stdin_ends_the_server_and_the_host() {
    let root = tempfile::TempDir::new().unwrap();
    let temp = tempfile::TempDir::new().unwrap();
    std::fs::write(root.path().join("gpui-pilot.toml"), config()).unwrap();
    let mut server = server_command(root.path(), temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let io = (server.stdout.take().unwrap(), server.stdin.take().unwrap());
    let client = ().serve(io).await.unwrap();
    let Value::Object(args) = launch_args() else {
        unreachable!("launch arguments are an object")
    };
    let launched = client
        .call_tool(CallToolRequestParams::new("launch").with_arguments(args))
        .await
        .unwrap();
    let host = launched.structured_content.unwrap()["pid"]
        .as_u64()
        .unwrap();
    assert!(alive(host));
    // Cancelling drops the client's transport, which closes the server's stdin.
    client.cancel().await.unwrap();
    let status = tokio::time::timeout(Duration::from_secs(10), server.wait())
        .await
        .expect("the server exits once its stdin closes")
        .unwrap();
    assert!(status.success(), "{status}");
    assert!(!alive(host), "the host went with the server");
}

#[tokio::test(flavor = "multi_thread")]
async fn waits_hovers_and_reports_hits_end_to_end() {
    let pilot = Pilot::start(Some(&config())).await;
    let mut launch = launch_args();
    launch["args"] = json!(["--boot"]);
    launch["env"] = json!({"PILOT_DEMO_GREETING": "hi"});
    let launched = pilot.ok("launch", launch).await;
    assert_eq!(launched["settled"], json!(true));
    let tree = pilot.ok("tree", json!({"settle_ms": 0})).await;
    let nodes = tree["nodes"].as_array().unwrap();
    assert!(
        nodes.iter().any(|n| ends_with(&n["path"], "booted")),
        "launch waited for the boot timer"
    );
    assert!(
        nodes.iter().any(|n| n["name"] == json!("hi")),
        "the launch env reached the app"
    );

    let covered = pilot
        .err("click", json!({"target": id("covered"), "timeout_ms": 200}))
        .await;
    assert_eq!(covered["code"], json!("Occluded"));
    assert!(ends_with(&covered["hit"]["path"], "cover"), "{covered}");

    pilot.ok("click", json!({"target": id("slow")})).await;
    let waited = pilot.ok("wait", json!({"target": id("slow-done")})).await;
    assert!(ends_with(&waited["node"]["path"], "slow-done"), "{waited}");
    let clicked = pilot.ok("click", json!({"target": id("slow-done")})).await;
    assert!(ends_with(&clicked["hit"]["path"], "slow-done"), "{clicked}");

    let hovered = pilot.ok("hover", json!({"target": id("count")})).await;
    assert!(ends_with(&hovered["hovered"]["path"], "count"), "{hovered}");
    let slept = pilot.ok("wait", json!({"ms": 50})).await;
    assert!(slept["waited_ms"].as_u64().unwrap() >= 50, "{slept}");
    assert_eq!(
        pilot.err("wait", json!({"ms": 60_000})).await["code"],
        json!("InvalidArgument")
    );
    let gone = pilot
        .err(
            "wait",
            json!({"target": id("count"), "gone": true, "timeout_ms": 100}),
        )
        .await;
    assert_eq!(gone["code"], json!("Timeout"));
    assert_eq!(pilot.ok("close", json!({})).await["exit_code"], json!(0));
    pilot.assert_bus_untouched();
}

/// The node whose path ends with `name` in a `tree` result.
fn node_named(tree: &Value, name: &str) -> Value {
    tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| ends_with(&n["path"], name))
        .unwrap_or_else(|| panic!("no `{name}` in the tree"))
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_multi_megabyte_value_comes_back_bounded_and_sliceable() {
    let pilot = Pilot::start(Some(&config())).await;
    let mut launch = launch_args();
    launch["args"] = json!(["--big-value", "3000000"]);
    pilot.ok("launch", launch).await;
    // The demo's big value is the digit `i % 10` at char `i`, so a slice reads its position.
    let tree = pilot.ok("tree", json!({})).await;
    let big = node_named(&tree, "big");
    assert_eq!(big["value_chars"], json!(3_000_000), "{big}");
    assert_eq!(big["value_cut"], json!(true));
    let shown = big["value"].as_str().unwrap();
    assert_eq!(shown.chars().count(), 801);
    assert!(shown.starts_with("0123456789") && shown.ends_with("0123456789"));

    let clicked = pilot.ok("click", json!({"target": id("big")})).await;
    assert_eq!(clicked["clicked"]["value_cut"], json!(true), "{clicked}");
    assert!(clicked["clicked"]["value"].as_str().unwrap().len() < 1000);

    let slice = json!({"within": id("big"), "value_slice": {"offset": 1_000_003, "length": 12}});
    let sliced = node_named(&pilot.ok("tree", slice).await, "big");
    assert_eq!(sliced["value"], json!("345678901234"), "{sliced}");
    assert_eq!(sliced["value_offset"], json!(1_000_003));
    let alone = json!({"value_slice": {"offset": 0, "length": 1}});
    assert_eq!(
        pilot.err("tree", alone).await["code"],
        json!("InvalidArgument")
    );

    pilot
        .ok("type", json!({"text": "short", "target": id("name")}))
        .await;
    let name = node_named(&pilot.ok("tree", json!({})).await, "name");
    assert_eq!(name["value"], json!("short"));
    assert!(name.get("value_chars").is_none() && name.get("value_cut").is_none());
    assert_eq!(pilot.ok("close", json!({})).await["exit_code"], json!(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn select_rows_are_clicked_by_an_id_suffix() {
    let pilot = Pilot::start(Some(&config())).await;
    pilot.ok("launch", launch_args()).await;
    pilot.ok("click", json!({"target": id("env")})).await;
    let missed = pilot
        .err("click", json!({"target": id("prod"), "timeout_ms": 0}))
        .await;
    assert_eq!(missed["code"], json!("ElementNotFound"));
    assert!(
        missed["message"].as_str().unwrap().contains("id_suffix"),
        "{missed}"
    );
    let clicked = pilot
        .ok("click", json!({"target": {"id_suffix": "-prod"}}))
        .await;
    assert!(
        ends_with(&clicked["clicked"]["path"], "env-option-prod"),
        "{clicked}"
    );
    let tree = pilot.ok("tree", json!({})).await;
    let env = tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| ends_with(&n["path"], "env"))
        .expect("the env select is in the tree")
        .clone();
    assert_eq!(env["value"], json!("prod"));
    assert_eq!(pilot.ok("close", json!({})).await["exit_code"], json!(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn keep_data_relaunches_on_the_same_data_until_the_server_exits() {
    let pilot = Pilot::start(Some(&config())).await;
    let mut args = launch_args();
    args["args"] = json!(["--persist"]);
    let first = pilot.ok("launch", args.clone()).await;
    assert_eq!(first["reused"], json!(false));
    let data_dir = PathBuf::from(first["data_dir"].as_str().unwrap());
    let clicks = data_dir.join("clicks");
    pilot.ok("click", json!({"target": id("count")})).await;
    pilot.ok("click", json!({"target": id("count")})).await;
    assert_eq!(std::fs::read_to_string(&clicks).unwrap(), "2");
    let home = data_dir.parent().unwrap().join("home");
    std::fs::write(home.join("marker"), "kept").unwrap();

    let closed = pilot.ok("close", json!({"keep_data": true})).await;
    assert_eq!(closed["data_dir"], json!(data_dir));
    let mut again = args.clone();
    again["keep_data"] = json!(true);
    let second = pilot.ok("launch", again.clone()).await;
    assert_eq!(second["reused"], json!(true));
    assert_eq!(second["data_dir"], json!(data_dir));
    pilot.ok("click", json!({"target": id("count")})).await;
    assert_eq!(
        std::fs::read_to_string(&clicks).unwrap(),
        "3",
        "the app restored its count"
    );
    assert_eq!(
        std::fs::read_to_string(home.join("marker")).unwrap(),
        "kept"
    );

    // One call relaunches a running app on its own data.
    again["replace"] = json!(true);
    let third = pilot.ok("launch", again).await;
    assert_eq!(third["reused"], json!(true));
    assert_eq!(third["data_dir"], json!(data_dir));
    assert_ne!(third["pid"], second["pid"]);
    pilot.ok("click", json!({"target": id("count")})).await;
    assert_eq!(std::fs::read_to_string(&clicks).unwrap(), "4");

    pilot.ok("close", json!({"keep_data": true})).await;
    assert!(data_dir.is_dir());
    // The server's TMPDIR stays alive, so only the server's exit can remove the kept dir.
    let Pilot { client, temp, .. } = pilot;
    client.cancel().await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while data_dir.parent().unwrap().exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !data_dir.parent().unwrap().exists(),
        "server exit removed the kept dir"
    );
    drop(temp);
}

/// The `files` drop target's label, read through `tree`.
async fn files_label(pilot: &Pilot) -> Value {
    let tree = pilot.ok("tree", json!({})).await;
    tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| ends_with(&n["path"], "files"))
        .expect("the files target is in the tree")["name"]
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn drops_external_paths_on_pilot_demo_end_to_end() {
    let pilot = Pilot::start(Some(&config())).await;
    pilot.ok("launch", launch_args()).await;
    let relative = pilot
        .err("drop", json!({"target": id("files"), "paths": ["a.json"]}))
        .await;
    assert_eq!(relative["code"], json!("InvalidArgument"));
    let paths = json!(["/tmp/import.json", "/tmp/a collection"]);
    let left = pilot
        .result(
            "drop",
            json!({"target": id("files"), "paths": paths, "cancel": true}),
        )
        .await;
    assert_eq!(
        left.structured_content.clone().unwrap()["dropped"],
        json!(false)
    );
    assert_eq!(files_label(&pilot).await, json!("Drop files"));
    let dropped = pilot
        .result("drop", json!({"target": id("files"), "paths": paths}))
        .await;
    let value = dropped.structured_content.clone().unwrap();
    assert!(ends_with(&value["over"]["path"], "files"), "{value}");
    assert!(
        common::text(&dropped).starts_with("drop: dropped files on"),
        "{}",
        common::text(&dropped)
    );
    assert_eq!(files_label(&pilot).await, json!("Files: 2"));
    pilot.ok("close", json!({})).await;
}

/// The `viewport` readout's label, read through `tree`.
async fn viewport_label(pilot: &Pilot) -> Value {
    let tree = pilot.ok("tree", json!({})).await;
    tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| ends_with(&n["path"], "viewport"))
        .expect("the viewport readout is in the tree")["name"]
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn resizes_pilot_demo_end_to_end() {
    let pilot = Pilot::start(Some(&config())).await;
    pilot.ok("launch", launch_args()).await;
    assert_eq!(viewport_label(&pilot).await, json!("Viewport 640x400"));
    let small = pilot
        .err("resize", json!({"width": 32, "height": 400}))
        .await;
    assert_eq!(small["code"], json!("InvalidArgument"));
    let resized = pilot
        .result("resize", json!({"width": 800, "height": 500}))
        .await;
    let value = resized.structured_content.clone().unwrap();
    assert_eq!(
        value["window"],
        json!({"w": 800.0, "h": 500.0, "scale": 1.0})
    );
    assert_eq!(value["settled"], json!(true));
    assert_eq!(
        common::text(&resized),
        "resize: window is 800x500 at scale 1; settled"
    );
    let shot = pilot.ok("screenshot", json!({})).await;
    assert_eq!((&shot["w"], &shot["h"]), (&json!(800), &json!(500)));
    assert_eq!(viewport_label(&pilot).await, json!("Viewport 800x500"));
    pilot.ok("close", json!({})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn drags_and_answers_gpui_dialogs_end_to_end() {
    let pilot = Pilot::start(Some(&config())).await;
    pilot.ok("launch", launch_args()).await;
    let dragged = pilot
        .result("drag", json!({"from": id("chip"), "to": id("dropzone")}))
        .await;
    let value = dragged.structured_content.clone().unwrap();
    assert!(ends_with(&value["to"]["path"], "dropzone"), "{value}");
    assert!(common::text(&dragged).starts_with("drag: dragged"));
    let tree = pilot.ok("tree", json!({})).await;
    let zone = tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| ends_with(&n["path"], "dropzone"))
        .expect("the drop zone is in the tree")
        .clone();
    assert_eq!(zone["name"], json!("Dropped 1"));

    assert_eq!(pilot.ok("dialog", json!({})).await.get("dialog"), None);
    pilot.ok("click", json!({"target": id("ask")})).await;
    let waiting = pilot.result("tree", json!({})).await;
    assert!(
        common::text(&waiting).contains(r#"dialog warning "Delete the chip?""#),
        "{}",
        common::text(&waiting)
    );
    let blocked = pilot.err("click", json!({"target": id("count")})).await;
    assert_eq!(blocked["code"], json!("Occluded"));
    let answered = pilot.ok("dialog", json!({"button": "Delete"})).await;
    assert_eq!(answered["answered"], json!("Delete"));
    assert_eq!(pilot.ok("dialog", json!({})).await.get("dialog"), None);
    assert_eq!(pilot.ok("close", json!({})).await["exit_code"], json!(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_scroll_zooms_and_a_window_close_honours_the_apps_prompt() {
    let pilot = Pilot::start(Some(&config())).await;
    let mut args = launch_args();
    args["args"] = json!(["--guard-close"]);
    pilot.ok("launch", args).await;
    let scrolled = pilot
        .ok(
            "scroll",
            json!({"target": id("name"), "dy": 40, "modifiers": ["ctrl"]}),
        )
        .await;
    assert_eq!(scrolled, json!({"settled": true}));
    let tree = pilot.ok("tree", json!({})).await;
    let zoom = tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| ends_with(&n["path"], "zoom"))
        .expect("the zoom button is in the tree")
        .clone();
    assert_eq!(zoom["name"], json!("Zoom 1"));

    let kept = pilot.ok("close", json!({"mode": "window"})).await;
    assert_eq!(kept, json!({"closed": false, "settled": true}));
    let waiting = pilot.result("tree", json!({})).await;
    assert!(
        common::text(&waiting).contains(r#"dialog warning "Discard unsaved changes?""#),
        "{}",
        common::text(&waiting)
    );
    let answered = pilot.ok("dialog", json!({"button": "Discard"})).await;
    assert_eq!(answered["answered"], json!("Discard"));
    assert_eq!(pilot.ok("close", json!({})).await["exit_code"], json!(0));

    pilot.ok("launch", launch_args()).await;
    let closed = pilot
        .ok("close", json!({"mode": "window", "keep_data": true}))
        .await;
    assert_eq!(
        (closed["closed"].clone(), closed["exit_code"].clone()),
        (json!(true), json!(0))
    );
    assert!(closed["data_dir"].is_string(), "{closed}");
    assert_eq!(
        pilot.err("tree", json!({})).await["code"],
        json!("NotRunning")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_demo_lists_and_writes_a_dir_and_fetches_from_a_local_server() {
    let shared = tempfile::tempdir().unwrap();
    std::fs::write(shared.path().join("hello.txt"), "hi").unwrap();
    let port = common::serve_once("hello");
    let pilot = Pilot::start(Some(&config())).await;
    let mut args = launch_args();
    args["args"] = json!([
        "--dir",
        shared.path(),
        "--fetch",
        format!("http://127.0.0.1:{port}/")
    ]);
    pilot.ok("launch", args).await;
    let button = |name: &str| json!({"role": "Button", "name": name});
    pilot
        .ok("tree", json!({"wait_for": button("listing: hello.txt")}))
        .await;
    pilot.ok("click", json!({"target": button("Touch")})).await;
    assert_eq!(
        std::fs::read_to_string(shared.path().join("touched")).unwrap(),
        "pilot-demo"
    );
    pilot.ok("click", json!({"target": button("Fetch")})).await;
    pilot
        .ok(
            "tree",
            json!({"wait_for": button("fetched: hello"), "timeout_ms": 5000}),
        )
        .await;
    pilot.ok("close", json!({})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_app_sees_whether_its_data_was_reused() {
    let pilot = Pilot::start(Some(&config())).await;
    let button = |name: &str| json!({"role": "Button", "name": name});
    let mut args = launch_args();
    args["args"] = json!(["--show-data"]);
    pilot.ok("launch", args.clone()).await;
    pilot
        .ok("tree", json!({"wait_for": button("data: fresh")}))
        .await;
    pilot.ok("close", json!({"keep_data": true})).await;
    let mut again = args.clone();
    again["keep_data"] = json!(true);
    assert_eq!(pilot.ok("launch", again).await["reused"], json!(true));
    pilot
        .ok("tree", json!({"wait_for": button("data: reused")}))
        .await;
    pilot.ok("close", json!({"keep_data": true})).await;
    // Without keep_data the kept dir is dropped and the app starts fresh.
    assert_eq!(pilot.ok("launch", args).await["reused"], json!(false));
    pilot
        .ok("tree", json!({"wait_for": button("data: fresh")}))
        .await;
    pilot.ok("close", json!({})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn launch_reports_the_rate_from_argument_config_or_default() {
    let pilot = Pilot::start(Some(&config())).await;
    assert_eq!(
        pilot.ok("launch", launch_args()).await["refresh_hz"],
        json!(144.0)
    );
    let mut sixty = launch_args();
    sixty["replace"] = json!(true);
    sixty["refresh_hz"] = json!(60);
    assert_eq!(pilot.ok("launch", sixty).await["refresh_hz"], json!(60.0));
    let mut odd = launch_args();
    odd["replace"] = json!(true);
    odd["refresh_hz"] = json!(75);
    assert_eq!(
        pilot.err("launch", odd).await["code"],
        json!("InvalidArgument")
    );
    pilot.ok("close", json!({})).await;

    let configured = Pilot::start(Some(&format!("{}refresh_hz = 120\n", config()))).await;
    assert_eq!(
        configured.ok("launch", launch_args()).await["refresh_hz"],
        json!(120.0)
    );
    configured.ok("close", json!({})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn frames_record_a_notch_scroll_and_report_the_build() {
    let pilot = Pilot::start(Some(&config())).await;
    let mut args = launch_args();
    args["refresh_hz"] = json!(60);
    args["args"] = json!(["--list"]);
    pilot.ok("launch", args.clone()).await;
    let stop = json!({"action": "stop"});
    let start = json!({"action": "start"});
    assert_eq!(
        pilot.err("frames", stop.clone()).await["code"],
        json!("InvalidArgument")
    );

    assert_eq!(
        pilot.ok("frames", start.clone()).await,
        json!({"refresh_mhz": 60_000})
    );
    assert_eq!(
        pilot.err("frames", start.clone()).await["code"],
        json!("InvalidArgument")
    );
    pilot.ok("wait", json!({"ms": 300})).await;
    let idle = pilot.ok("frames", stop.clone()).await;
    assert_eq!(idle["frames"], json!(0));
    assert!(idle.get("intervals").is_none(), "nulls are dropped: {idle}");
    assert_eq!(
        idle["build"],
        json!({"opt_level": "0", "debug_assertions": true})
    );
    assert_eq!(idle["warnings"], json!(["unoptimised_host"]));

    let middle = || json!({"x": 320, "y": 200});
    for bad in [
        json!({"target": middle(), "notches": 3, "dy": 40}),
        json!({"target": middle(), "notches": 201}),
        json!({"target": middle(), "notches": 3, "notch_ms": 0}),
    ] {
        assert_eq!(
            pilot.err("scroll", bad).await["code"],
            json!("InvalidArgument")
        );
    }

    pilot.ok("frames", start.clone()).await;
    let scrolled = pilot
        .ok(
            "scroll",
            json!({"target": middle(), "notches": 30, "notch_ms": 17, "settle_ms": 0}),
        )
        .await;
    assert!(scrolled.get("settled").is_some(), "{scrolled}");
    let result = pilot.result("frames", stop.clone()).await;
    let report = result
        .structured_content
        .clone()
        .expect("structured content");
    let load = common::load::report();
    assert!(report["frames"].as_u64().unwrap() >= 25, "{report}; {load}");
    assert!(
        report["latency"]["count"].as_u64().unwrap() >= 25,
        "{report}; {load}"
    );
    assert!(report["intervals"]["median_ms"].is_number(), "{report}");
    assert!(
        common::text(&result).starts_with("frames: "),
        "{}",
        common::text(&result)
    );

    // A relaunch discards the recording; the new app has none to stop.
    pilot.ok("frames", start).await;
    args["replace"] = json!(true);
    pilot.ok("launch", args).await;
    assert_eq!(
        pilot.err("frames", stop.clone()).await["code"],
        json!("InvalidArgument")
    );
    pilot.ok("close", json!({})).await;
    assert_eq!(pilot.err("frames", stop).await["code"], json!("NotRunning"));
}

/// A host that speaks only protocol `version`, as one built from `kit-0.7.1-p11` (7) or `p14`
/// (8): it refuses any other `Hello` version with `ProtocolMismatch`, worded as real hosts word
/// it, and exits; it answers a matching `Hello`, `click` and `close`, and logs every request
/// line it receives to `requests` beside its socket.
fn old_host(dir: &std::path::Path, version: u32) -> PathBuf {
    use gpui_pilot_protocol::{ErrorCode, Node, Outcome, PilotError, Rect, Reply, WindowInfo};
    let mut node = Node::new(Vec::new(), Rect::new(0.0, 0.0, 1.0, 1.0), true, 0);
    node.id = Some("count".into());
    let canned = json!({
        "hello": serde_json::to_value(Outcome::Ok(Reply::Hello {
            version,
            pid: 1,
            window: WindowInfo::new(640.0, 400.0, 1.0),
            settled: true,
            refresh_mhz: 144_000,
        })).unwrap(),
        "mismatch": serde_json::to_value(Outcome::Err(PilotError::new(
            ErrorCode::ProtocolMismatch,
            format!("the server speaks version {PROTOCOL_VERSION}, this host speaks {version}"),
        ))).unwrap(),
        "click": serde_json::to_value(Outcome::Ok(Reply::Click {
            clicked: node.clone(),
            hit: node,
            settled: true,
        })).unwrap(),
        "close": serde_json::to_value(Outcome::Ok(Reply::Closed)).unwrap(),
        "version": version,
    });
    std::fs::write(dir.join("canned.json"), canned.to_string()).unwrap();
    let script = dir.join("host7.py");
    std::fs::write(
        &script,
        r#"#!/usr/bin/python3
import json, os, socket, sys
here = os.path.dirname(os.path.abspath(__file__))
canned = json.load(open(os.path.join(here, "canned.json")))
log = open(os.path.join(here, "requests"), "a")
s = socket.socket(socket.AF_UNIX)
s.connect(os.environ["GPUI_PILOT_SOCKET"])
f = s.makefile("rw")
for line in f:
    log.write(line)
    log.flush()
    frame = json.loads(line)
    req = frame["request"]
    if req["op"] == "hello":
        if req["version"] != canned["version"]:
            f.write(json.dumps({"id": frame["id"], "outcome": canned["mismatch"]}) + "\n")
            f.flush()
            sys.exit(3)
        out = canned["hello"]
    elif req["op"] in canned:
        out = canned[req["op"]]
    else:
        sys.exit(4)
    f.write(json.dumps({"id": frame["id"], "outcome": out}) + "\n")
    f.flush()
    if req["op"] == "close":
        break
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[tokio::test(flavor = "multi_thread")]
async fn a_protocol_seven_host_still_launches_and_clicks_and_modifiers_are_refused_for_it() {
    let scripts = tempfile::tempdir().unwrap();
    let host = old_host(scripts.path(), 7);
    let pilot = Pilot::start(Some(&format!(
        "[headless]\nbinary = \"{}\"\n",
        host.display()
    )))
    .await;
    // The first Hello (this server's version) is refused and the host exits; the retry speaks
    // the 7 the refusal named.
    pilot.ok("launch", launch_args()).await;
    let clicked = pilot.ok("click", json!({"target": id("count")})).await;
    assert_eq!(clicked["settled"], json!(true));
    for (tool, args) in [
        (
            "click",
            json!({"target": id("count"), "modifiers": ["ctrl"]}),
        ),
        (
            "drag",
            json!({"from": id("count"), "to": id("count"), "modifiers": ["shift"]}),
        ),
    ] {
        let refused = pilot.err(tool, args).await;
        assert_eq!(refused["code"], json!("Unsupported"));
        let text = refused["message"].as_str().unwrap();
        assert!(
            text.contains("kit-0.7.1-p12") && text.contains("protocol 7"),
            "{text}"
        );
    }
    // A right drag is refused the same way, and never reaches the host.
    let refused = pilot
        .err(
            "drag",
            json!({"from": id("count"), "to": id("count"), "button": "right"}),
        )
        .await;
    assert_eq!(refused["code"], json!("Unsupported"));
    let text = refused["message"].as_str().unwrap();
    assert!(
        text.contains("kit-0.7.1-p15") && text.contains("protocol 7"),
        "{text}"
    );
    pilot.ok("close", json!({})).await;
    let requests = std::fs::read_to_string(scripts.path().join("requests")).unwrap();
    assert!(!requests.contains("modifiers"), "{requests}");
    assert!(!requests.contains(r#""op":"drag""#), "{requests}");
    assert!(
        requests.contains(&format!(r#""version":{PROTOCOL_VERSION}"#)),
        "this server's hello came first"
    );
    assert!(!requests.contains(r#""version":8"#), "{requests}");
    assert_eq!(requests.matches(r#""op":"click""#).count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_protocol_eight_host_still_launches_and_clicks_and_a_right_drag_is_refused_for_it() {
    let scripts = tempfile::tempdir().unwrap();
    let host = old_host(scripts.path(), 8);
    let pilot = Pilot::start(Some(&format!(
        "[headless]\nbinary = \"{}\"\n",
        host.display()
    )))
    .await;
    // The first Hello is refused and the host exits; the retry speaks the 8 it named.
    pilot.ok("launch", launch_args()).await;
    let clicked = pilot
        .ok(
            "click",
            json!({"target": id("count"), "modifiers": ["ctrl"]}),
        )
        .await;
    assert_eq!(clicked["settled"], json!(true));
    let refused = pilot
        .err(
            "drag",
            json!({"from": id("count"), "to": id("count"), "button": "middle"}),
        )
        .await;
    assert_eq!(refused["code"], json!("Unsupported"));
    let text = refused["message"].as_str().unwrap();
    assert!(
        text.contains("kit-0.7.1-p15") && text.contains("protocol 8"),
        "{text}"
    );
    pilot.ok("close", json!({})).await;
    let requests = std::fs::read_to_string(scripts.path().join("requests")).unwrap();
    assert!(requests.contains(r#""version":8"#), "{requests}");
    assert!(!requests.contains(r#""version":7"#), "{requests}");
    assert!(!requests.contains(r#""op":"drag""#), "{requests}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_protocol_nine_host_still_launches_and_clicks_and_a_drop_is_refused_for_it() {
    let scripts = tempfile::tempdir().unwrap();
    let host = old_host(scripts.path(), 9);
    let pilot = Pilot::start(Some(&format!(
        "[headless]\nbinary = \"{}\"\n",
        host.display()
    )))
    .await;
    // The first Hello is refused and the host exits; the retry speaks the 9 it named.
    pilot.ok("launch", launch_args()).await;
    let clicked = pilot.ok("click", json!({"target": id("count")})).await;
    assert_eq!(clicked["settled"], json!(true));
    let refused = pilot
        .err("drop", json!({"target": id("count"), "paths": ["/tmp/a"]}))
        .await;
    assert_eq!(refused["code"], json!("Unsupported"));
    let text = refused["message"].as_str().unwrap();
    assert!(
        text.contains("kit-0.7.1-p17") && text.contains("protocol 9"),
        "{text}"
    );
    pilot.ok("close", json!({})).await;
    let requests = std::fs::read_to_string(scripts.path().join("requests")).unwrap();
    assert!(requests.contains(r#""version":9"#), "{requests}");
    assert!(!requests.contains(r#""op":"drop""#), "{requests}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_protocol_eight_host_accepts_modifiers_on_click_and_drag() {
    let pilot = Pilot::start(Some(&config())).await;
    pilot.ok("launch", launch_args()).await;
    let clicked = pilot
        .ok(
            "click",
            json!({"target": id("count"), "modifiers": ["ctrl", "shift"]}),
        )
        .await;
    assert_eq!(clicked["settled"], json!(true));
    let dragged = pilot
        .ok(
            "drag",
            json!({"from": id("chip"), "to": id("dropzone"), "modifiers": ["cmd"]}),
        )
        .await;
    assert_eq!(dragged["settled"], json!(true));
    pilot.ok("close", json!({})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_type_into_a_slow_app_completes_while_a_short_one_is_unchanged() {
    let pilot = Pilot::start(Some(&config())).await;
    let mut launch = launch_args();
    // Each key costs a 100 ms frame, so 320 keys outlast the 30 s reply margin of a call
    // with no timeout or settle of its own.
    launch["args"] = json!(["--render-delay-ms", "100"]);
    pilot.ok("launch", launch).await;
    let short = pilot
        .ok("type", json!({"text": "abc", "target": id("name")}))
        .await;
    assert_eq!(short, json!({"settled": true}));
    let text = "x".repeat(320);
    let long = pilot
        .ok(
            "type",
            json!({"text": text, "timeout_ms": 0, "settle_ms": 0}),
        )
        .await;
    // A zero settle budget reports unsettled; the call itself must not time out.
    assert!(long.get("settled").is_some(), "{long}");
    let name = node_named(&pilot.ok("tree", json!({})).await, "name");
    assert_eq!(name["value"].as_str().map(str::len), Some(323));
    pilot.ok("close", json!({})).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_protocol_ten_host_still_launches_and_clicks_and_a_resize_is_refused_for_it() {
    let scripts = tempfile::tempdir().unwrap();
    let host = old_host(scripts.path(), 10);
    let pilot = Pilot::start(Some(&format!(
        "[headless]\nbinary = \"{}\"\n",
        host.display()
    )))
    .await;
    // The first Hello is refused and the host exits; the retry speaks the 10 it named.
    pilot.ok("launch", launch_args()).await;
    let clicked = pilot.ok("click", json!({"target": id("count")})).await;
    assert_eq!(clicked["settled"], json!(true));
    let refused = pilot
        .err("resize", json!({"width": 800, "height": 500}))
        .await;
    assert_eq!(refused["code"], json!("Unsupported"));
    let text = refused["message"].as_str().unwrap();
    assert!(
        text.contains("kit-0.7.1-p18") && text.contains("protocol 10"),
        "{text}"
    );
    pilot.ok("close", json!({})).await;
    let requests = std::fs::read_to_string(scripts.path().join("requests")).unwrap();
    assert!(requests.contains(r#""version":10"#), "{requests}");
    assert!(!requests.contains(r#""op":"resize""#), "{requests}");
}
