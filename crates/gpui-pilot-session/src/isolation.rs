//! The negative connect check: a session refuses to start while any desktop socket is
//! reachable from inside its sandbox.

use std::fmt;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::PathBuf;

use crate::SessionError;

/// A Unix socket address the sandbox must not reach.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Address {
    /// A filesystem socket.
    Path(PathBuf),
    /// A Linux abstract socket name, without the leading NUL.
    Abstract(String),
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Address::Path(path) => write!(f, "{}", path.display()),
            Address::Abstract(name) => write!(f, "@{name}"),
        }
    }
}

/// The desktop's sockets for `uid`: the user's session bus and the abstract X sockets
/// of displays 0 and 1, which a shared network namespace would expose.
pub fn desktop_addresses(uid: u32) -> Vec<Address> {
    vec![
        Address::Path(format!("/run/user/{uid}/bus").into()),
        Address::Abstract("/tmp/.X11-unix/X0".into()),
        Address::Abstract("/tmp/.X11-unix/X1".into()),
    ]
}

/// Tries to connect to each address and returns `IsolationBreach` naming the first one
/// that accepts. A missing or refusing socket passes; only a successful connect is a breach.
pub fn check(addresses: &[Address]) -> Result<(), SessionError> {
    for address in addresses {
        if connects(address) {
            return Err(SessionError::IsolationBreach {
                address: address.to_string(),
            });
        }
    }
    Ok(())
}

/// `check` on this process's own desktop sockets: the guard before anything joins a bus.
pub fn check_desktop() -> Result<(), SessionError> {
    check(&desktop_addresses(rustix::process::getuid().as_raw()))
}

// The stream is dropped at once: reaching the socket is the whole finding.
fn connects(address: &Address) -> bool {
    match address {
        Address::Path(path) => UnixStream::connect(path).is_ok(),
        Address::Abstract(name) => SocketAddr::from_abstract_name(name.as_bytes())
            .and_then(|addr| UnixStream::connect_addr(&addr))
            .is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_pilot_protocol::ErrorCode;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};

    fn short_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("gp-")
            .tempdir_in("/tmp")
            .unwrap()
    }

    #[test]
    fn missing_and_refused_addresses_pass() {
        let dir = short_dir();
        let name = format!("gpui-pilot-test-none-{}", std::process::id());
        check(&[
            Address::Path(dir.path().join("nothing.sock")),
            Address::Abstract(name),
        ])
        .unwrap();
    }

    #[test]
    fn a_listening_path_is_a_breach_naming_it() {
        let dir = short_dir();
        let path = dir.path().join("bus");
        let _listener = UnixListener::bind(&path).unwrap();
        let error = check(&[Address::Path(path.clone())]).unwrap_err();
        assert_eq!(error.code(), ErrorCode::IsolationBreach);
        match error {
            SessionError::IsolationBreach { address } => {
                assert_eq!(address, path.display().to_string())
            }
            other => panic!("{other}"),
        }
    }

    #[test]
    fn a_listening_abstract_name_is_a_breach() {
        let name = format!("gpui-pilot-test-{}", std::process::id());
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let _listener = UnixListener::bind_addr(&addr).unwrap();
        match check(&[Address::Abstract(name.clone())]).unwrap_err() {
            SessionError::IsolationBreach { address } => assert_eq!(address, format!("@{name}")),
            other => panic!("{other}"),
        }
    }

    #[test]
    fn the_desktop_addresses_are_the_user_bus_and_both_x_sockets() {
        assert_eq!(
            desktop_addresses(1000),
            vec![
                Address::Path("/run/user/1000/bus".into()),
                Address::Abstract("/tmp/.X11-unix/X0".into()),
                Address::Abstract("/tmp/.X11-unix/X1".into()),
            ]
        );
    }
}
