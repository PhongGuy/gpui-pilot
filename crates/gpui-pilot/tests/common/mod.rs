//! `Pilot`: a `gpui-pilot` server process in a scratch project, driven by rmcp's client over
//! stdio with a cleared environment.
#![allow(dead_code)] // each test binary uses a different subset

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use base64::Engine as _;

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use tempfile::TempDir;
use tokio::process::Command;

/// Machine load for timing assertions; see `test-support/load.rs`.
#[path = "../../../../test-support/load.rs"]
pub mod load;

pub struct Pilot {
    pub client: RunningService<RoleClient, ()>,
    pub server_pid: u32,
    /// The server's working directory, holding `gpui-pilot.toml`.
    pub root: TempDir,
    /// The server's `TMPDIR`, where launch directories go.
    pub temp: TempDir,
    /// Stand-in session bus handed to the server; the host must never receive its address.
    pub bus: UnixListener,
}

impl Pilot {
    /// Starts the server in a fresh project whose `gpui-pilot.toml` is `config`, if any.
    pub async fn start(config: Option<&str>) -> Pilot {
        Self::start_with(Path::new(env!("CARGO_BIN_EXE_gpui-pilot")), config).await
    }

    /// As `start`, but runs the server binary at `server`.
    pub async fn start_with(server: &Path, config: Option<&str>) -> Pilot {
        let root = TempDir::new().unwrap();
        let temp = TempDir::new().unwrap();
        if let Some(text) = config {
            std::fs::write(root.path().join("gpui-pilot.toml"), text).unwrap();
        }
        let bus_path = temp.path().join("session_bus");
        let bus = UnixListener::bind(&bus_path).unwrap();
        bus.set_nonblocking(true).unwrap();
        let mut command = server_command_at(server, root.path(), temp.path());
        command
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", bus_path.display()),
            )
            .env("DISPLAY", ":99")
            .env("WAYLAND_DISPLAY", "gpui-pilot-canary");
        let transport = TokioChildProcess::new(command).unwrap();
        let server_pid = transport.id().unwrap();
        let client = ().serve(transport).await.unwrap();
        Pilot {
            client,
            server_pid,
            root,
            temp,
            bus,
        }
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// The listed tool names, sorted.
    pub async fn tools(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .client
            .list_all_tools()
            .await
            .unwrap()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        names
    }

    /// `Err` is a JSON-RPC error, not an `isError` result.
    pub async fn try_call(&self, tool: &str, args: Value) -> Result<CallToolResult, String> {
        let Value::Object(args) = args else {
            panic!("tool arguments must be an object")
        };
        self.client
            .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(args))
            .await
            .map_err(|e| e.to_string())
    }

    /// Calls `tool`, requires success, and returns the whole result.
    pub async fn result(&self, tool: &str, args: Value) -> CallToolResult {
        let result = self
            .try_call(tool, args)
            .await
            .unwrap_or_else(|e| panic!("{tool}: {e}"));
        assert_ne!(
            result.is_error,
            Some(true),
            "{tool} failed: {:?}",
            result.structured_content
        );
        result
    }

    /// Calls `tool`, requires success, and returns its structured content.
    pub async fn ok(&self, tool: &str, args: Value) -> Value {
        self.result(tool, args)
            .await
            .structured_content
            .expect("structured content")
    }

    /// Calls `tool`, requires an `isError` result, and returns its structured content.
    pub async fn err(&self, tool: &str, args: Value) -> Value {
        let result = self
            .try_call(tool, args)
            .await
            .unwrap_or_else(|e| panic!("{tool}: {e}"));
        assert_eq!(
            result.is_error,
            Some(true),
            "{tool} succeeded: {:?}",
            result.structured_content
        );
        result.structured_content.expect("structured error")
    }

    /// Asserts nothing connected to the stand-in session bus.
    pub fn assert_bus_untouched(&self) {
        match self.bus.accept() {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("something connected to the session bus: {other:?}"),
        }
    }
}

/// The server binary with a cleared environment, `root` as its working directory and `temp`
/// as its `TMPDIR`.
pub fn server_command(root: &Path, temp: &Path) -> Command {
    server_command_at(Path::new(env!("CARGO_BIN_EXE_gpui-pilot")), root, temp)
}

/// As `server_command`, but for the server binary at `binary`.
pub fn server_command_at(binary: &Path, root: &Path, temp: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("TMPDIR", temp)
        .current_dir(root);
    command
}

/// The text blocks of a result, joined by newlines.
pub fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The decoded PNG of a `screenshot` result.
pub fn png(result: &CallToolResult) -> Vec<u8> {
    let image = result
        .content
        .iter()
        .find_map(|c| c.as_image())
        .expect("an image block");
    assert_eq!(image.mime_type, "image/png");
    base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .unwrap()
}

/// Whether `pid` names a live process; a zombie counts as gone.
pub fn alive(pid: u64) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !matches!(
            stat.rsplit(") ").next().and_then(|s| s.chars().next()),
            Some('Z' | 'X')
        ),
        Err(_) => false,
    }
}

pub fn kill(pid: u64) {
    let status = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill -9 {pid}");
}

/// pilot-demo's binary, built next to this crate's by `cargo test --workspace`.
pub fn pilot_demo() -> PathBuf {
    let path = Path::new(env!("CARGO_BIN_EXE_gpui-pilot")).with_file_name("pilot-demo");
    assert!(
        path.exists(),
        "{} is missing; run `cargo build -p pilot-demo` first",
        path.display()
    );
    path
}

/// Serves one HTTP/1.0 request on 127.0.0.1 with `200 OK` and `body`; returns the port.
pub fn serve_once(body: &'static str) -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request);
        let reply = format!(
            "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(reply.as_bytes()).unwrap();
    });
    port
}
