//! The app opt-in contract and `run`: connect to the server's socket, take the `Hello`
//! handshake, launch the app on a `PilotPlatform`, and answer requests until `Close` or
//! end of stream.

use std::{
    borrow::Cow, cell::Cell, io::BufReader, os::unix::net::UnixStream, path::PathBuf,
    process::ExitCode, rc::Rc, sync::Arc, time::Duration,
};

use futures::{StreamExt as _, channel::mpsc};
use gpui_kit::{App, Application, AssetSource, Pixels, SharedString, Size, px, size};
use gpui_pilot_protocol::{
    BackendFrame, ClientFrame, ErrorCode, LaunchParams, Outcome, PROTOCOL_VERSION, PilotError,
    REFRESH_RATES_HZ, Reply, Request, WindowInfo, read_line, read_line_lenient, write_line,
};

use crate::{Pilot, PilotPlatform, settle::settle};

/// What an app provides to be piloted: its assets, its global setup, and its main window.
pub trait PilotApp: 'static {
    fn assets(&self) -> Option<Arc<dyn AssetSource>>;
    fn init(&self, cx: &mut App);
    fn open(&self, launch: &Launch, cx: &mut App) -> anyhow::Result<()>;
}

/// The launch parameters an app's `open` sees, built from the server's `Hello`.
#[non_exhaustive]
pub struct Launch {
    pub data_dir: PathBuf,
    pub size: Size<Pixels>,
    pub scale: f32,
    pub args: Vec<String>,
    /// The data dir is one a previous launch kept; the app should keep its own saved state.
    pub reused: bool,
}

/// Lets a shared asset source be handed to `Application::with_assets`, which wants a value.
struct SharedAssets(Arc<dyn AssetSource>);

impl AssetSource for SharedAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        self.0.load(path)
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        self.0.list(path)
    }
}

const BAD_SETUP: u8 = 2;
const VERSION_MISMATCH: u8 = 3;
const LAUNCH_FAILED: u8 = 1;

/// Runs `app` under the pilot until the server closes it; the exit code says how it ended.
/// Call it first thing in `main`: it rewrites the D-Bus addresses, which is only sound while
/// the process has one thread, and exits 2 otherwise. Exits 2 without a socket or a `Hello` or
/// on a rate outside `REFRESH_RATES_HZ`, 3 on a protocol mismatch, 1 if `open` fails.
pub fn run(app: impl PilotApp) -> ExitCode {
    if let Err(error) = isolate_from_buses() {
        eprintln!("gpui-pilot: {error}");
        return ExitCode::from(BAD_SETUP);
    }
    let Some(path) = std::env::var_os("GPUI_PILOT_SOCKET") else {
        eprintln!("gpui-pilot: GPUI_PILOT_SOCKET is not set; start this binary from the pilot");
        return ExitCode::from(BAD_SETUP);
    };
    let (mut writer, mut reader) = match connect(&path) {
        Ok(ends) => ends,
        Err(error) => {
            eprintln!("gpui-pilot: connecting to {}: {error}", path.display());
            return ExitCode::from(BAD_SETUP);
        }
    };
    let (id, launch) = match handshake(&mut reader) {
        Ok(hello) => hello,
        Err((id, error, code)) => {
            write_line(&mut writer, &BackendFrame::new(id, Outcome::Err(error))).ok();
            return ExitCode::from(code);
        }
    };

    let platform = PilotPlatform::new(
        size(px(launch.width), px(launch.height)),
        launch.scale,
        launch.refresh_hz,
    );
    platform.set_settle_horizon(Duration::from_millis(launch.settle_horizon_ms));
    let window = WindowInfo::new(launch.width, launch.height, launch.scale);
    let settle_ms = launch.settle_ms;
    let launch = Launch {
        data_dir: launch.data_dir,
        size: size(px(launch.width), px(launch.height)),
        scale: launch.scale,
        args: launch.args,
        reused: launch.reused,
    };
    let mut application = Application::with_platform(platform.clone());
    if let Some(assets) = app.assets() {
        application = application.with_assets(SharedAssets(assets));
    }
    let exit = Rc::new(Cell::new(0));
    let exit_code = exit.clone();
    application.run(move |cx| {
        app.init(cx);
        if let Err(error) = app.open(&launch, cx) {
            let error = PilotError::new(ErrorCode::LaunchFailed, format!("{error:#}"));
            write_line(&mut writer, &BackendFrame::new(id, Outcome::Err(error))).ok();
            exit_code.set(LAUNCH_FAILED);
            cx.quit();
            return;
        }
        serve(platform, reader, writer, id, window, settle_ms, cx);
    });
    ExitCode::from(exit.get())
}

/// Points both D-Bus addresses at nothing, so code no platform override reaches (the GPU
/// driver, the kit's portal queries) fails fast instead of touching the user's buses. The platform overrides stay the guard
/// for GPUI's own bus calls, pinned by the in-process tests.
fn isolate_from_buses() -> Result<(), String> {
    single_threaded()?;
    // SAFETY: the process has a single thread, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", NO_BUS);
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", NO_BUS);
    }
    Ok(())
}

/// A socket path that cannot exist, so a connect fails at once.
const NO_BUS: &str = "unix:path=/nonexistent/gpui-pilot-no-bus";

/// Errs unless the calling thread is the only one, read from `/proc/self/status`.
fn single_threaded() -> Result<(), String> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|error| format!("reading /proc/self/status: {error}"))?;
    let threads = status
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .map(str::trim);
    match threads {
        Some("1") => Ok(()),
        other => Err(format!(
            "the process already has {} threads; call run first thing in main",
            other.unwrap_or("an unknown number of")
        )),
    }
}

fn connect(path: &std::ffi::OsStr) -> std::io::Result<(UnixStream, BufReader<UnixStream>)> {
    let stream = UnixStream::connect(path)?;
    let reader = BufReader::new(stream.try_clone()?);
    Ok((stream, reader))
}

/// Reads the first frame, which must be a `Hello` at this protocol version. A failure carries
/// the id to answer, the error, and the exit code.
fn handshake(
    reader: &mut BufReader<UnixStream>,
) -> Result<(u64, LaunchParams), (u64, PilotError, u8)> {
    let internal =
        |id, message: String| (id, PilotError::new(ErrorCode::Internal, message), BAD_SETUP);
    let frame: ClientFrame = match read_line(reader) {
        Ok(Some(frame)) => frame,
        Ok(None) => return Err(internal(0, "the stream ended before hello".into())),
        Err(error) => return Err(internal(0, format!("reading hello: {error}"))),
    };
    match frame.request {
        // A rate outside the measured set never reaches the platform, so no period is zero.
        Request::Hello { version, launch }
            if version == PROTOCOL_VERSION && !REFRESH_RATES_HZ.contains(&launch.refresh_hz) =>
        {
            Err((
                frame.id,
                PilotError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "refresh_hz {} is not one of {REFRESH_RATES_HZ:?}",
                        launch.refresh_hz
                    ),
                ),
                BAD_SETUP,
            ))
        }
        Request::Hello { version, launch } if version == PROTOCOL_VERSION => Ok((frame.id, launch)),
        Request::Hello { version, .. } => Err((
            frame.id,
            PilotError::new(
                ErrorCode::ProtocolMismatch,
                format!("the server speaks version {version}, this host speaks {PROTOCOL_VERSION}"),
            ),
            VERSION_MISMATCH,
        )),
        _ => Err(internal(frame.id, "the first request must be hello".into())),
    }
}

/// Answers the `Hello` with `hello_id` once the main window settles (within `settle_ms`),
/// then feeds frames from a reader thread to a foreground task that answers them in order.
/// The app quits on `Close`, on a `CloseWindow` that closed, when the app itself quit during
/// a request (after that reply), at end of stream, or when a reply
/// cannot be written.
fn serve(
    platform: Rc<PilotPlatform>,
    mut reader: BufReader<UnixStream>,
    mut writer: UnixStream,
    hello_id: u64,
    window: WindowInfo,
    settle_ms: u64,
    cx: &mut App,
) {
    let (frames, mut incoming) = mpsc::unbounded::<ClientFrame>();
    let mut rejects = writer.try_clone().ok();
    std::thread::spawn(move || {
        // A frame this host cannot parse (a request from a newer server) is answered with an
        // error and skipped, so one unknown request does not end the session.
        while let Ok(Some(read)) = read_line_lenient::<ClientFrame>(&mut reader) {
            match read {
                Ok(frame) => {
                    if frames.unbounded_send(frame).is_err() {
                        break;
                    }
                }
                Err(bad) => {
                    let error = PilotError::new(
                        ErrorCode::InvalidArgument,
                        format!(
                            "this host (protocol {PROTOCOL_VERSION}) cannot read the request: {}; \
                             it may need a newer host",
                            bad.message
                        ),
                    );
                    let reply = BackendFrame::new(bad.id.unwrap_or(0), Outcome::Err(error));
                    if rejects
                        .as_mut()
                        .is_none_or(|out| write_line(out, &reply).is_err())
                    {
                        break;
                    }
                }
            }
        }
    });
    cx.spawn(async move |cx| {
        // The app's `open` only schedules its loading; settling first means the agent's
        // first request sees the loaded window rather than racing it.
        let settled = match platform.main_window() {
            Some(main) => {
                let budget = Duration::from_millis(settle_ms);
                settle(&platform, &main, cx, budget).await.settled
            }
            None => false,
        };
        let hello = Reply::Hello {
            version: PROTOCOL_VERSION,
            pid: std::process::id(),
            window,
            settled,
            refresh_mhz: platform.refresh_hz() * 1000,
        };
        if write_line(
            &mut writer,
            &BackendFrame::new(hello_id, Outcome::Ok(hello)),
        )
        .is_err()
        {
            cx.update(|cx| cx.quit());
            return;
        }
        let pilot = Pilot::new(platform.clone());
        while let Some(frame) = incoming.next().await {
            let closing = matches!(frame.request, Request::Close);
            platform.begin_request();
            let outcome = match frame.request {
                Request::Close => Outcome::Ok(Reply::Closed),
                Request::Hello { .. } => Outcome::Err(PilotError::new(
                    ErrorCode::Internal,
                    "hello was already answered",
                )),
                request => pilot.handle(request, cx).await,
            };
            let window_closed = matches!(
                outcome,
                Outcome::Ok(Reply::CloseWindow { closed: true, .. })
            );
            let written = write_line(&mut writer, &BackendFrame::new(frame.id, outcome));
            // A quit the app asked for during the request ends the loop now that its reply
            // is out; an agreed window close runs GPUI's close path only after the reply too.
            let quit_asked = platform.end_request();
            if window_closed {
                pilot.finish_window_close(cx);
            }
            if closing || window_closed || quit_asked || written.is_err() {
                break;
            }
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::single_threaded;

    #[test]
    fn the_bus_override_refuses_a_process_with_other_threads() {
        let (started, wait) = std::sync::mpsc::channel();
        let (release, parked) = std::sync::mpsc::channel::<()>();
        let other = std::thread::spawn(move || {
            started.send(()).unwrap();
            parked.recv().ok();
        });
        wait.recv().unwrap();
        let checked = single_threaded();
        release.send(()).unwrap();
        other.join().unwrap();
        assert!(
            checked
                .unwrap_err()
                .contains("call run first thing in main")
        );
    }
}
