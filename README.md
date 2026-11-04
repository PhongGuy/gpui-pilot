# gpui-pilot

gpui-pilot lets an AI coding agent run a real [GPUI](https://www.gpui.rs) desktop app and use it the way a person would: take screenshots, read the element tree, click, type, press keys, scroll, drag, and answer dialogs and file pickers. It is an MCP server, so any agent that speaks the Model Context Protocol can use it.

The point is to let an agent check its own UI work. After changing a view, the agent launches the app, looks at it, drives it through the change and confirms the result, without a person watching and without touching the person's desktop. The app runs either headless (no display at all) or inside a private, sandboxed desktop session that the user never sees.

It targets apps built on [gpui-kit](https://crates.io/crates/gpui-kit) and runs on Linux.

## How it works

```
 agent (MCP client)
      |  MCP over stdio
      v
 gpui-pilot  (the server: reads gpui-pilot.toml, builds and launches the app)
      |  JSON lines over a Unix socket, protocol version 11
      +--> headless: your pilot binary, which runs the app on gpui-pilot-host
      +--> session:  your normal app binary, in a private KWin session inside a bwrap sandbox
```

There are two parts:

- **The server**, the `gpui-pilot` binary. An agent starts it as `gpui-pilot mcp` and talks MCP to it over stdin and stdout. It reads `gpui-pilot.toml` from its working directory, runs the configured build command, launches the app, and turns each tool call into a request to the app. It runs one app at a time.
- **The host**, the `gpui-pilot-host` crate. An app embeds it in a small extra binary for headless mode. The host runs the app on its own GPUI platform: windows have no surface, every frame is rendered offscreen on the GPU, and input arrives as GPUI events. It answers the server's requests (element tree, input, screenshots, prompts) over the socket.

### Headless mode

The server starts your pilot binary (the one that calls `gpui_pilot_host::run`) as a child process. The app never connects to a display server: it gets no `DISPLAY` or `WAYLAND_DISPLAY`, its environment is cleared down to `PATH`, `LANG`, `RUST_LOG` and `RUST_BACKTRACE` (plus any `env` the launch passes), and it gets a private `HOME`, `XDG_*` directories and `TMPDIR` under `/tmp/gpui-pilot-<uid>/`. The host also points both D-Bus addresses at a socket that does not exist, so nothing in the process (the GPU driver, portal queries) reaches the user's session or system bus.

Headless mode shares the host's network. It is fast, needs no desktop, and is the mode to use for most checks.

### Session mode

The server starts your normal debug app binary inside a private KDE session: a `kwin_wayland` compositor on a virtual output, its own D-Bus session bus, the AT-SPI accessibility registry and the KDE file dialog portal, all inside a `bwrap` sandbox. The sandbox has its own PID and IPC namespaces, a fresh `/tmp`, `/run` and `/run/user`, read-only `/usr`, `/etc` and `/sys`, and access to the GPU devices. Before the app starts, the server checks that the user's session bus and X11 sockets cannot be reached from inside, and refuses to run if they can.

The element tree comes from AT-SPI, screenshots from KWin, and input from KWin's emulated input. This mode exercises the real Wayland path: real window placement, the real KDE file dialog (`pick_file`) and the real clipboard.

The network in session mode is `none` by default: no network at all. With `pasta`, the app gets a user-mode network stack, and `launch {"host_ports": [8080]}` lets it reach a server on the host's `127.0.0.1:8080` and nothing else on the host's loopback.

Host files are made visible inside the sandbox with `binds` in the config. Launch `args` that are absolute paths to existing files or folders are bound read-write at the same path, except device and kernel paths, Unix sockets, and anything that would expose the desktop session.

### Protocol version

The server and host agree on a protocol version in their first message. This release speaks protocol 11. The server still launches hosts back to protocol 7: it retries at the version the host names and answers `Unsupported` for the features an older host lacks (`resize` needs 11, `drop` 10, a right or middle `drag` 9, `modifiers` on `click` and `drag` 8). To use a newer feature, move the app's `gpui-pilot-host` to the matching tag and rebuild the pilot binary.

Releases are git tags with a semver version, for example `v0.1.0`. Each release states the gpui-kit version its host is built against; v0.1.0 is built against gpui-kit 0.7.1. Use the same tag for the server and the host.

## Requirements

- Linux. The host builds on GPUI's Linux platform, and the server uses Unix sockets and Linux namespaces. macOS and Windows are not supported.
- Rust 1.99 or newer.
- A GPU that GPUI's wgpu renderer can use, plus the system libraries GPUI needs on Linux. Headless mode renders on the GPU even with no display.
- An app on gpui-kit 0.7.1, the version the host pins.
- For session mode, also:
  - `bwrap` (bubblewrap), with unprivileged user namespaces enabled;
  - `kwin_wayland` (KDE Plasma 6) and `dbus-run-session`;
  - `/usr/lib/at-spi2-registryd`, `/usr/lib/at-spi-bus-launcher` and `/usr/share/dbus-1/session.conf`;
  - `pasta` (from passt), only when the network is `pasta`;
  - `xdg-desktop-portal` with the KDE backend, for `pick_file`.

  The server checks all but the portal before each session launch and names the first one missing (`EnvironmentMissing`). The AT-SPI paths are fixed; a distribution that installs those binaries elsewhere is not handled yet.

## Install the server

```sh
cargo install --locked --git https://github.com/PhongGuy/gpui-pilot --tag v0.1.0 gpui-pilot
```

This installs `gpui-pilot` into `~/.cargo/bin`. `gpui-pilot mcp` (or `gpui-pilot` with no argument) serves MCP over stdio.

## Add the server to an agent

The server reads `gpui-pilot.toml` from its working directory, so start it from the app's project root.

### Claude Code

Add a `.mcp.json` at the project root:

```json
{
  "mcpServers": {
    "gpui-pilot": {
      "command": "gpui-pilot",
      "args": ["mcp"]
    }
  }
}
```

Or let Claude Code write it:

```sh
claude mcp add --scope project gpui-pilot -- gpui-pilot mcp
```

Without `--scope project`, the entry is kept to you (the `local` scope) instead of going into `.mcp.json`.

### Other MCP clients

Any client that launches stdio servers works. Give it the command `gpui-pilot` with the argument `mcp`, and the project root as the working directory. Most clients take the same shape:

```json
{
  "mcpServers": {
    "gpui-pilot": { "command": "gpui-pilot", "args": ["mcp"] }
  }
}
```

`launch` runs the build command first, which can take a while on a cold build. If your client has a per-call timeout, build once by hand first or raise the timeout.

## Add it to a GPUI app

### Headless: a pilot binary

`gpui-pilot-host` is debug-only: it turns on gpui-kit's `test-support` feature and refuses to compile in a release profile. To keep that feature out of your real app's build, put the pilot binary in a small crate of its own, for example `tools/my-app-pilot/`, with its own `[workspace]` so cargo does not merge its features into your workspace:

```toml
[package]
name = "my-app-pilot"
version = "0.1.0"
edition = "2024"
publish = false

# Its own workspace root, so its features never reach the app's workspace.
[workspace]

[dependencies]
my-app = { path = "../../crates/my-app" }
gpui-kit = "=0.7.1"
gpui-pilot-host = { git = "https://github.com/PhongGuy/gpui-pilot", tag = "v0.1.0" }
anyhow = "1"

# Optional: optimised dependencies make the app much faster under the pilot.
[profile.dev.package."*"]
opt-level = 1
```

The binary implements `PilotApp` and hands it to `run`:

```rust
use std::process::ExitCode;
use std::sync::Arc;

use gpui_kit::{App, AssetSource};
use gpui_pilot_host::{Launch, PilotApp};

struct MyApp;

impl PilotApp for MyApp {
    // The app's assets (icons, fonts), or None.
    fn assets(&self) -> Option<Arc<dyn AssetSource>> {
        Some(Arc::new(gpui_kit::assets::AllAssets))
    }

    // Global setup, as the app's own main does before opening a window.
    fn init(&self, cx: &mut App) {
        gpui_kit::init(cx);
        my_app::init(cx);
    }

    // Open the main window; the pilot decides its size and scale.
    fn open(&self, launch: &Launch, cx: &mut App) -> anyhow::Result<()> {
        my_app::open_main_window(&launch.args, cx)
    }
}

fn main() -> ExitCode {
    // First thing in main: run refuses to start once the process has other threads.
    gpui_pilot_host::run(MyApp)
}
```

`my_app::init` and `my_app::open_main_window` stand for your app's own setup and window code. `Launch` carries:

- `args`: the config's `args` followed by the `args` of the `launch` call;
- `data_dir`: a private directory for this launch;
- `size` and `scale`: the window size in logical pixels and the display scale;
- `reused`: true when the agent relaunched on kept data (`launch {"keep_data": true}`), so the app should restore what it saved instead of seeding fresh state.

`run` exits 2 when started outside the pilot, 3 on a protocol mismatch and 1 when `open` fails; the server passes the error and the log tail to the agent.

### Session mode

Session mode needs no code: it runs the app's normal debug binary. The tree comes from accessibility, so elements are as findable as the app makes them for a screen reader.

### gpui-pilot.toml

Put this file at the project root. Both sections are optional, but a launch in a mode needs that mode's section.

```toml
[headless]
build = ["cargo", "build", "--manifest-path", "tools/my-app-pilot/Cargo.toml"]
binary = "tools/my-app-pilot/target/debug/my-app-pilot"

[session]
build = ["cargo", "build", "--bin", "my-app"]
binary = "target/debug/my-app"
binds = ["fixtures", "/srv/test-data:rw"]
network = "pasta"
refresh_hz = 60
```

| Key | Meaning |
| --- | --- |
| `binary` | Required. The program to launch, relative to the project root or absolute. It must be a debug build: a path with a `release` component is refused. |
| `build` | The build command as an argument list, run in the project root before each launch unless the launch passes `build: false`. Empty by default. |
| `args` | Arguments always passed to the app, before the launch call's own `args`. |
| `binds` | Used in session mode. Host paths mounted into the sandbox at the same path, read-only unless suffixed `:rw`. A relative path names a path in the project; it may not hold `..` or leave the project through a symlink. Paths that would expose the desktop session (`/run`, `/tmp/.X11-unix` and the like) are refused. |
| `network` | Session only: `none` (the default) or `pasta`. Headless always uses the host network, and setting it there is an error. |
| `refresh_hz` | The display refresh rate: 60, 120, 144 or 240. Default 144. In session mode, rates other than 60 accept only window widths the display timing can represent, and `launch` names the nearest widths that work. |

Unknown keys are an error, so a typo is reported rather than ignored. If you set `CARGO_TARGET_DIR`, point `binary` at the directory it names.

## Tools

Most tools take a target, which is one of:

- `{"id": ["sidebar", "save"]}`: an element path. The last segment is the element's own id and earlier ones are its ancestors in order, with gaps allowed. Segments are strings or non-negative integers.
- `{"id_suffix": "save"}`: an element whose own id ends with this text.
- `{"role": "Button", "name": "Save", "nth": 0}`: the nth element with this role and exact name (`nth` defaults to 0).
- `{"x": 120, "y": 40}`: a point in logical pixels relative to the window's content.

Actions wait for the target to be actionable (`timeout_ms`), then for the app to settle (`settle_ms`), before they reply.

| Tool | What it does |
| --- | --- |
| `launch` | Build and start the configured app in `headless` or `session` mode, with window size, scale, `args`, `env`, network, `host_ports`, `refresh_hz` and `keep_data`. |
| `tree` | List the elements with their paths, roles, names, values, states and bounds, optionally only `within` one element; `max_chars` shortens long runs of similar rows. |
| `screenshot` | Capture the window, or one element, as a PNG. |
| `click` | Click an element or point, with a button, a click count and held modifiers. |
| `type` | Type text, optionally clicking a target first; `redact: true` keeps the text out of replies. |
| `key` | Press a keystroke such as `enter` or `ctrl-s`. |
| `scroll` | Scroll by logical pixels or by mouse wheel notches, with held modifiers. |
| `hover` | Move the pointer over an element or point. |
| `drag` | Press on one target, move in steps and release on another, with any mouse button and held modifiers. |
| `drop` | Drop files or folders from outside onto a target, as a file manager would. Headless only. |
| `resize` | Resize the window's content to a size in logical pixels without moving the pointer. |
| `frames` | Start and stop a recording of the app's frames; the report gives frame intervals, frames over budget and wheel input latency. |
| `dialog` | Read the GPUI prompt the app is showing, or answer it with a button label. |
| `pick_file` | Answer the app's open or save file dialog with paths, or cancel it. |
| `clipboard` | Read the clipboard text, or replace it. |
| `wait` | Wait for an element to appear or disappear, or wait a number of milliseconds. |
| `close` | Close the app, by killing it or by closing its window as a window manager would (so the app can prompt), optionally keeping its data for the next launch. |

Each tool's full parameters and defaults are in the schema the server publishes over MCP, which an agent reads when it connects.

Errors carry a code such as `ElementNotFound` (with the nearest matches), `Ambiguous`, `Timeout`, `AppCrashed` (with the log tail), `BuildFailed`, `InvalidConfig`, `EnvironmentMissing` or `Unsupported`, so an agent can tell what went wrong.

## Driving by hand

`scripts/drive.py` starts the server as `<server> mcp` and runs one call per stdin line, `<tool> [json args]` (blank lines and `#` lines are skipped). It prints each call, its text and its structured content, saves screenshots as `shot-N.png` and each `tree` result as `tree-N.json`, and exits 1 if any call failed.

```sh
printf '%s\n' 'launch {"mode": "headless"}' \
  'tree' \
  'click {"target": {"role": "Button", "name": "Save"}}' \
  screenshot | scripts/drive.py --out shots
```

`--server PATH` names the server binary (default `$GPUI_PILOT`, else `gpui-pilot` on `PATH`); `--out DIR` is where files go (default `.`). It needs only python3. Run it from the project root, as an agent would.

## Known limits

- Linux only, and session mode needs KDE Plasma 6's KWin.
- The headless host and the session binary must be debug builds.
- One app per server at a time; launching another while one runs needs `replace: true`.
- `drop` is headless only.
- Headless mode shares the host's network; use session mode with `none` or `pasta` when the app must not reach it.
- `frames` numbers are only meaningful when the app's dependencies are built optimised.
- The host pins gpui-kit 0.7.1; an app on another gpui-kit version needs a host release built for that version.

## Licence

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or the [MIT licence](LICENSE-MIT), at your option.
