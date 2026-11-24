# Contributing to gpui-pilot

This is the developer guide; the page for people who use gpui-pilot is [README.md](README.md).

## Sending a change

Issues and pull requests from anyone are welcome, and a first PR is fine: you don't need to ask before opening one. Before I merge, I check that:

- the commands under [Build and check](#build-and-check) pass, with fmt and clippy clean;
- the PR makes one change, with tests for new behaviour;
- the README is updated when the change is something a user would look for.

If something is missing, I'll say what and help get the PR over the line; a PR that isn't finished yet is a fine place to ask. For a larger change, opening an issue first lets us agree on the shape before you write the code.

## Requirements

- Linux, Rust 1.99 or newer (the `rust-version` in `Cargo.toml`) and a GPU that GPUI's wgpu renderer can use.
- The system libraries CI installs, on Debian or Ubuntu:

```sh
sudo apt-get install -y --no-install-recommends pkg-config libfontconfig-dev libfreetype-dev \
  libwayland-dev libx11-xcb-dev libxkbcommon-x11-dev libvulkan1 libegl1
```

- For the session tests, the session-mode tools listed under [Requirements](README.md#requirements) in the README.

## Layout

- `crates/gpui-pilot`: the server binary. It speaks MCP to the agent, reads `gpui-pilot.toml`, builds and launches the app, and never links GPUI.
- `crates/gpui-pilot-protocol`: the JSON-lines protocol between the server and a backend, and its version.
- `crates/gpui-pilot-host`: the headless backend an app embeds; the only crate that depends on GPUI.
- `crates/gpui-pilot-session`: the session backend (bwrap sandbox, KWin, EIS input, AT-SPI tree, the KDE portal).
- `crates/pilot-demo`: a small gpui-kit app the tests drive in both modes.
- `test-support/`: helpers shared by the test suites.
- `scripts/drive.py`: drives the server by hand (see [Driving by hand](README.md#driving-by-hand)).

## Build and check

Run these in order before opening a PR:

```sh
cargo build -p gpui-pilot -p pilot-demo --bins --profile timing
cargo build -p gpui-pilot -p pilot-demo --bins
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -Dwarnings
cargo test --workspace
```

`make check` runs the last three. The server's tests launch `target/debug/pilot-demo` and the timing tests launch the `timing` builds, and neither builds them, so build both profiles first; a missing binary fails with the command to run.

Tests never reach your desktop: `.cargo/config.toml` points both D-Bus addresses at a socket that does not exist, and nothing connects to your display.

### Session tests

Session tests are opt-in. Without the variable they pass while running nothing:

```sh
GPUI_PILOT_SESSION_TESTS=1 cargo test --workspace -- --test-threads=4
```

Each test starts its own KWin inside the sandbox; more than four at once miss KWin's start timeout. Run them for anything that touches session mode, and for a gpui-kit move.

### Timing tests

A timing test's failure message ends with the machine's load. If it says the machine was overloaded, rerun that test alone with `-- --test-threads=1` and report both runs; a miss on a quiet machine is a regression. `GPUI_PILOT_SKIP_TIMING=1` skips the timing budgets.

## CI

CI is a backstop, not a substitute for running the checks yourself. The `gate` job runs fmt, clippy, both build profiles and the whole suite on mesa's software Vulkan driver (lavapipe) with the timing budgets skipped. `zizmor` audits the workflows, and `attribution` fails a PR whose title, body or commits credit an AI agent. Session tests never run in CI.

## Conventions

- Commits follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/): `<type>(<scope>): <description>`, with `feat`, `fix`, `docs`, `test`, `refactor` or `chore`.
- One issue, one branch, one PR. The PR body says `Closes #N` and carries the issue's label. PRs are merged with a merge commit, never squashed.
- Comments explain intent and the one non-obvious constraint, not what the code says.
- Public types in the library crates are `#[non_exhaustive]`, and `gpui-pilot-protocol` exposes no dependency type.
- `gpui-pilot-host` pins `gpui-kit` and every `gpui-pre-*` crate it names with `=`.
- Any change to the wire format bumps `PROTOCOL_VERSION`. The exception is an optional field that older hosts never receive, or an optional reply field the server accepts missing.

## Issues and pull requests

File bugs and requests as GitHub issues. For a bug, give the tool call, its arguments, the reply, the mode (headless or session), and the `gpui-pilot --version` and host tag in use. For a request, describe the problem the agent hit, not only the fix you have in mind.

Before sending a PR, run the checks above and put their totals in the PR body, plus the session-test totals when session mode is touched. Change the README in the same PR when the change is something a user would look for.

## Releases

Releases are `vX.Y.Z` git tags; there are no release packages, since users install with `cargo install --git --tag`. A move to a new gpui-kit is a minor version, because apps have to move with it. Published tags never move. Before each tag, the README is checked against everything merged since the last one.
