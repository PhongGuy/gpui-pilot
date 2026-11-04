//! The session clipboard through `ext_data_control_v1`, only ever on the private Wayland socket.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use wl_clipboard_rs::{copy, paste};

use crate::SessionError;
use crate::sandbox::WAYLAND_SOCKET;

/// `Internal` unless `value` is `sandbox::WAYLAND_SOCKET`.
pub(crate) fn check_display(value: Option<&OsStr>) -> Result<(), SessionError> {
    if value == Some(OsStr::new(WAYLAND_SOCKET)) {
        return Ok(());
    }
    Err(SessionError::Internal(format!(
        "the clipboard connects only to the private Wayland socket {WAYLAND_SOCKET:?}, not {value:?}"
    )))
}

/// The guard before every connect. wayland-client prefers an inherited `WAYLAND_SOCKET` fd
/// over `WAYLAND_DISPLAY`, so a set one is refused too.
fn check_env() -> Result<(), SessionError> {
    if std::env::var_os("WAYLAND_SOCKET").is_some() {
        return Err(SessionError::Internal(
            "the clipboard refuses an inherited WAYLAND_SOCKET".into(),
        ));
    }
    check_display(std::env::var_os("WAYLAND_DISPLAY").as_deref())
}

/// Replaces the selection with `text` (the text types plus `next_marker()`), served from a
/// background thread until another client takes it, and returns once the compositor offers it.
/// `Unsupported` without data control; `Timeout` after `OFFER_TIMEOUT`.
pub(crate) fn set_text(text: &str) -> Result<(), SessionError> {
    check_env()?;
    // The copy returns before its set_selection is flushed, so wait until the compositor lists
    // this call's marker; a text compare would match a stale offer of the same text.
    let marker = next_marker();
    let sources = vec![
        copy::MimeSource {
            source: copy::Source::Bytes(text.as_bytes().into()),
            mime_type: copy::MimeType::Text,
        },
        copy::MimeSource {
            source: copy::Source::Bytes(Box::default()),
            mime_type: copy::MimeType::Specific(marker.clone()),
        },
    ];
    match copy::Options::new().copy_multi(sources) {
        Ok(()) => {}
        Err(copy::Error::MissingProtocol { name, version }) => return Err(missing(name, version)),
        Err(error) => {
            return Err(SessionError::Internal(format!(
                "setting the clipboard: {error}"
            )));
        }
    }
    wait_offered(&marker, OFFER_TIMEOUT, offered_types)
}

/// The current selection's MIME types; empty clipboard or no seat is the empty set.
fn offered_types() -> Result<HashSet<String>, SessionError> {
    check_env()?;
    match paste::get_mime_types(paste::ClipboardType::Regular, paste::Seat::Unspecified) {
        Ok(types) => Ok(types),
        Err(paste::Error::ClipboardEmpty | paste::Error::NoSeats) => Ok(HashSet::new()),
        Err(paste::Error::MissingProtocol { name, version }) => Err(missing(name, version)),
        Err(error) => Err(SessionError::Internal(format!(
            "reading the clipboard offer: {error}"
        ))),
    }
}

/// The selection as text; empty, no seat or no text type is `None`.
pub(crate) fn get_text() -> Result<Option<String>, SessionError> {
    check_env()?;
    let got = paste::get_contents(
        paste::ClipboardType::Regular,
        paste::Seat::Unspecified,
        paste::MimeType::Text,
    );
    let (mut pipe, _) = match got {
        Ok(contents) => contents,
        Err(paste::Error::ClipboardEmpty | paste::Error::NoSeats | paste::Error::NoMimeType) => {
            return Ok(None);
        }
        Err(paste::Error::MissingProtocol { name, version }) => return Err(missing(name, version)),
        Err(error) => {
            return Err(SessionError::Internal(format!(
                "reading the clipboard: {error}"
            )));
        }
    };
    let mut text = String::new();
    pipe.read_to_string(&mut text)
        .map_err(|e| SessionError::Internal(format!("reading the clipboard: {e}")))?;
    Ok(Some(text))
}

/// How long `set_text` waits for the compositor to offer the new selection.
const OFFER_TIMEOUT: Duration = Duration::from_secs(2);
const OFFER_POLL: Duration = Duration::from_millis(10);

/// A MIME type unique to this call (`application/x-gpui-pilot-offer-<pid>-<n>`), never `text/*`.
fn next_marker() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("application/x-gpui-pilot-offer-{}-{n}", std::process::id())
}

/// Polls `offered` every `OFFER_POLL` until its set holds `marker`. `Timeout` after `timeout`;
/// an error from `offered` ends the wait at once and is returned.
fn wait_offered(
    marker: &str,
    timeout: Duration,
    mut offered: impl FnMut() -> Result<HashSet<String>, SessionError>,
) -> Result<(), SessionError> {
    // The deadline is checked after each probe, so a probe slower than the timeout still runs once.
    let deadline = Instant::now() + timeout;
    loop {
        if offered()?.contains(marker) {
            return Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(SessionError::Timeout(format!(
                "the session compositor did not offer the new clipboard within {} s; the clipboard was not confirmed set and nothing was pasted after it",
                timeout.as_secs_f64()
            )));
        }
        std::thread::sleep(OFFER_POLL.min(deadline - now));
    }
}

fn missing(name: &str, version: u32) -> SessionError {
    SessionError::Unsupported(format!(
        "the session compositor offers no {name} v{version}, so it has no clipboard access"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_pilot_protocol::ErrorCode;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    #[test]
    fn only_the_private_wayland_socket_is_accepted() {
        assert!(check_display(Some(OsStr::new("gpui-pilot"))).is_ok());
        for other in [Some(OsStr::new("wayland-0")), Some(OsStr::new("")), None] {
            let error = check_display(other).unwrap_err();
            assert!(matches!(error, SessionError::Internal(_)), "{error}");
        }
        assert_eq!(
            SessionError::Unsupported("x".into()).code(),
            ErrorCode::Unsupported
        );
    }

    fn types(list: &[&str]) -> HashSet<String> {
        list.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn the_wait_ends_once_the_compositor_offers_this_calls_marker() {
        let marker = next_marker();
        let mut calls = 0;
        let waited = wait_offered(&marker, Duration::from_secs(1), || {
            calls += 1;
            // The previous selection first, as when set_selection is not yet flushed.
            Ok(if calls < 3 {
                types(&["text/plain;charset=utf-8", "text/plain"])
            } else {
                types(&["text/plain;charset=utf-8", marker.as_str()])
            })
        });
        assert!(waited.is_ok(), "{waited:?}");
        assert_eq!(calls, 3);
    }

    #[test]
    fn a_stale_offer_times_out_with_a_clear_error() {
        let marker = next_marker();
        let stale = next_marker();
        let started = Instant::now();
        let error = wait_offered(&marker, Duration::from_millis(150), || {
            Ok(types(&["text/plain;charset=utf-8", stale.as_str()]))
        })
        .unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(150) && elapsed < Duration::from_secs(1),
            "{elapsed:?}"
        );
        match error {
            SessionError::Timeout(message) => assert!(
                message.contains("did not offer the new clipboard")
                    && message.contains("was not confirmed set"),
                "{message}"
            ),
            other => panic!("not a timeout: {other}"),
        }
    }

    #[test]
    fn a_probe_error_ends_the_wait_at_once() {
        let mut calls = 0;
        let error = wait_offered(&next_marker(), Duration::from_secs(1), || {
            calls += 1;
            Err(SessionError::Unsupported("no data control".into()))
        })
        .unwrap_err();
        assert!(matches!(error, SessionError::Unsupported(_)), "{error}");
        assert_eq!(calls, 1);
    }

    #[test]
    fn each_call_gets_its_own_marker_that_no_app_reads_as_text() {
        let (a, b) = (next_marker(), next_marker());
        assert_ne!(a, b);
        for marker in [&a, &b] {
            assert!(
                marker.starts_with("application/x-gpui-pilot-offer-"),
                "{marker}"
            );
            assert!(!wl_clipboard_rs::utils::is_text(marker), "{marker}");
        }
    }
}
