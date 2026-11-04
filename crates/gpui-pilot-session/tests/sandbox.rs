//! The sandbox from the inside: the desktop is unreachable, and a planted socket is caught.

mod common;

use std::os::unix::net::UnixListener;

use common::{OUTPUT, inner, run_inside, session_tests, uid};
use gpui_pilot_session::SessionError;
use gpui_pilot_session::isolation::{check, desktop_addresses};
use gpui_pilot_session::sandbox::Bind;

#[test]
fn the_desktop_is_unreachable_inside() {
    if !session_tests() {
        return;
    }
    let out = run_inside("inner_desktop_unreachable", OUTPUT, |_| {});
    assert!(out.contains("isolation ok"), "{out}");
}

#[test]
#[ignore = "runs inside the sandbox through the_desktop_is_unreachable_inside"]
fn inner_desktop_unreachable() {
    if !inner() {
        return;
    }
    check(&desktop_addresses(uid())).unwrap();
    println!("isolation ok");
}

#[test]
fn a_socket_planted_at_the_user_bus_path_is_a_breach() {
    if !session_tests() {
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("gp-fake-")
        .tempdir_in("/tmp")
        .unwrap();
    let fake = dir.path().join("bus");
    let _listener = UnixListener::bind(&fake).unwrap();
    let bus = format!("/run/user/{}/bus", uid());
    let out = run_inside("inner_breach_reported", OUTPUT, |s| {
        s.binds.push(Bind::at(&fake, &bus))
    });
    assert!(out.contains(&format!("breach {bus}")), "{out}");
}

#[test]
#[ignore = "runs inside the sandbox through a_socket_planted_at_the_user_bus_path_is_a_breach"]
fn inner_breach_reported() {
    if !inner() {
        return;
    }
    match check(&desktop_addresses(uid())).unwrap_err() {
        SessionError::IsolationBreach { address } => println!("breach {address}"),
        other => panic!("{other}"),
    }
}
