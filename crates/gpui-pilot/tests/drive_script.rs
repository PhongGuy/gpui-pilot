//! scripts/drive.py against pilot-demo: the hand-driving script shows text and structured content.

mod common;

use std::io::Write as _;
use std::process::{Command, Stdio};

use common::pilot_demo;

#[test]
fn the_drive_script_prints_text_and_structured_content() {
    let root = tempfile::TempDir::new().unwrap();
    let temp = tempfile::TempDir::new().unwrap();
    let out = tempfile::TempDir::new().unwrap();
    let config = format!("[headless]\nbinary = \"{}\"\n", pilot_demo().display());
    std::fs::write(root.path().join("gpui-pilot.toml"), config).unwrap();
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/drive.py");
    let mut child = Command::new("python3")
        .arg(script)
        .arg("--server")
        .arg(env!("CARGO_BIN_EXE_gpui-pilot"))
        .arg("--out")
        .arg(out.path())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.path())
        .env("TMPDIR", temp.path())
        .current_dir(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 runs scripts/drive.py");
    let commands = concat!(
        "# comments and blank lines are skipped\n\n",
        "launch {\"mode\": \"headless\", \"width\": 640, \"height\": 400, \"build\": false}\n",
        "click {\"target\": {\"id\": [\"count\"]}}\n",
        "clipboard {\"set\": \"drive\"}\n",
        "screenshot\n",
        "tree\n",
        "click {\"target\": {\"id\": [\"no-such-id\"]}, \"timeout_ms\": 0}\n",
        "close\n",
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(commands.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "one call fails on purpose\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("> click {\"target\": {\"id\": [\"count\"]}}"),
        "{stdout}"
    );
    assert!(
        stdout.contains("click: clicked [\"count\"] Button"),
        "{stdout}"
    );
    assert!(stdout.contains("clipboard: \"drive\"; settled"), "{stdout}");
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("structured: {") && l.contains("\"settled\":true")),
        "{stdout}"
    );
    assert!(stdout.contains("ElementNotFound: "), "{stdout}");
    assert!(out.path().join("shot-1.png").is_file());
    assert!(out.path().join("tree-1.json").is_file());
}

/// Runs scripts/drive.py over `commands` against pilot-demo; returns its stdout and `--out` dir.
fn drive(commands: &str) -> (String, tempfile::TempDir) {
    let root = tempfile::TempDir::new().unwrap();
    let temp = tempfile::TempDir::new().unwrap();
    let out = tempfile::TempDir::new().unwrap();
    let config = format!("[headless]\nbinary = \"{}\"\n", pilot_demo().display());
    std::fs::write(root.path().join("gpui-pilot.toml"), config).unwrap();
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/drive.py");
    let mut child = Command::new("python3")
        .arg(script)
        .arg("--server")
        .arg(env!("CARGO_BIN_EXE_gpui-pilot"))
        .arg("--out")
        .arg(out.path())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.path())
        .env("TMPDIR", temp.path())
        .current_dir(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 runs scripts/drive.py");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(commands.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    (String::from_utf8_lossy(&output.stdout).into_owned(), out)
}

/// Every three-character run of `secret` that appears in `log`.
fn leaks(log: &str, secret: &str) -> Vec<String> {
    let chars: Vec<char> = secret.chars().collect();
    chars
        .windows(3)
        .map(|w| w.iter().collect::<String>())
        .filter(|part| log.contains(part.as_str()))
        .collect()
}

#[test]
fn the_drive_log_never_holds_text_typed_into_a_masked_or_redacted_input() {
    // Masked is detected by the host; `redact` covers a plain input; a failed call cannot
    // know what it would have typed into, so it is redacted too.
    let masked = "qZ7#xK9!w";
    let asked = "Pv4%mJ8&r";
    let failed = "Ty6^nH3*u";
    let commands = format!(
        concat!(
            "launch {{\"mode\": \"headless\", \"width\": 640, \"height\": 400, \"build\": false, \"args\": [\"--secret\"]}}\n",
            "type {{\"text\": \"{masked}\", \"target\": {{\"id\": [\"secret\"]}}}}\n",
            "type {{\"text\": \"{asked}\", \"target\": {{\"id\": [\"name\"]}}, \"redact\": true}}\n",
            "type {{\"text\": \"{failed}\", \"target\": {{\"id\": [\"no-such-id\"]}}, \"timeout_ms\": 0}}\n",
            "type {{\"text\": \"plain\"}}\n",
            "tree\n",
            "close\n",
        ),
        masked = masked,
        asked = asked,
        failed = failed,
    );
    let (stdout, out) = drive(&commands);
    let tree = std::fs::read_to_string(out.path().join("tree-1.json")).unwrap();
    for secret in [masked, asked, failed] {
        assert_eq!(leaks(&stdout, secret), Vec::<String>::new(), "{stdout}");
        assert_eq!(leaks(&tree, secret), Vec::<String>::new(), "{tree}");
    }
    assert!(
        stdout.contains(
            "> type {\"text\": \"<redacted 9 chars>\", \"target\": {\"id\": [\"secret\"]}}"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("type: typed <redacted 9 chars>; settled"),
        "{stdout}"
    );
    assert!(stdout.contains("\"masked\":true"), "{stdout}");
    assert!(stdout.contains("ElementNotFound: "), "{stdout}");
    assert!(stdout.contains("> type {\"text\": \"plain\"}"), "{stdout}");
}
