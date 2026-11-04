//! The private compositor's output refresh, read back through `wl_output`.

use std::os::unix::net::UnixStream;
use std::path::Path;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

use crate::SessionError;

/// The refresh of the last mode the output marked current, in mHz.
#[derive(Default)]
struct Modes(Option<i32>);

impl Dispatch<WlRegistry, GlobalListContents> for Modes {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for Modes {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode {
            flags: WEnum::Value(flags),
            refresh,
            ..
        } = event
            && flags.contains(wl_output::Mode::Current)
        {
            state.0 = Some(refresh);
        }
    }
}

/// The current mode's refresh of the first `wl_output` on the compositor at `socket`, in mHz.
/// `LaunchFailed` stage `output` when the socket, the global or the current mode is missing.
pub(crate) fn read_refresh_mhz(socket: &Path) -> Result<u32, SessionError> {
    let fail = |what: &str, error: &dyn std::fmt::Display| SessionError::LaunchFailed {
        stage: "output",
        message: format!("{what}: {error}"),
    };
    let stream = UnixStream::connect(socket).map_err(|e| fail("connect", &e))?;
    let conn = Connection::from_socket(stream).map_err(|e| fail("wayland connection", &e))?;
    let (globals, mut queue) =
        registry_queue_init::<Modes>(&conn).map_err(|e| fail("registry", &e))?;
    // The bind's events (geometry, modes, done) arrive within one more roundtrip.
    let _output: WlOutput = globals
        .bind(&queue.handle(), 1..=4, ())
        .map_err(|e| fail("wl_output", &e))?;
    let mut modes = Modes::default();
    queue
        .roundtrip(&mut modes)
        .map_err(|e| fail("roundtrip", &e))?;
    let refresh = modes
        .0
        .ok_or_else(|| fail("wl_output", &"no current mode"))?;
    u32::try_from(refresh).map_err(|e| fail("refresh", &e))
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::read_refresh_mhz;
    use crate::SessionError;

    fn stage(error: SessionError) -> &'static str {
        match error {
            SessionError::LaunchFailed { stage, .. } => stage,
            other => panic!("not a launch failure: {other:?}"),
        }
    }

    #[test]
    fn a_missing_socket_is_a_launch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let error = read_refresh_mhz(&dir.path().join("gpui-pilot")).unwrap_err();
        assert_eq!(stage(error), "output");
    }

    #[test]
    fn a_compositor_that_hangs_up_is_a_launch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpui-pilot");
        let listener = UnixListener::bind(&path).unwrap();
        // Accept and drop at once, as a compositor dying mid-handshake would.
        let closer = std::thread::spawn(move || drop(listener.accept()));
        let error = read_refresh_mhz(&path).unwrap_err();
        closer.join().unwrap();
        assert_eq!(stage(error), "output");
    }
}
