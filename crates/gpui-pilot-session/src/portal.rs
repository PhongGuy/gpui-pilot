//! The portal watch: a D-Bus monitor on the private session bus that records every FileChooser
//! call the app makes and every response it receives, plus the helpers that read that log.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gpui_pilot_protocol::{PromptInfo, PromptKind};
use zbus::MatchRule;
use zbus::blocking::{Connection, MessageIterator, fdo::MonitoringProxy};
use zbus::message::Type;
use zbus::zvariant::OwnedValue;

use crate::SessionError;
use crate::isolation::{Address, check, desktop_addresses};

/// The calls and responses the watch records (spike §5).
const RULES: [&str; 2] = [
    "type='method_call',interface='org.freedesktop.portal.FileChooser'",
    "type='signal',interface='org.freedesktop.portal.Request',member='Response'",
];

/// What the monitor saw on the private bus, in order.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub enum PortalEvent {
    Prompt(PromptInfo),
    Response { code: u32, uris: Vec<String> },
}

/// A monitor connection on the private session bus and the events its thread has read.
pub struct PortalWatch {
    events: Arc<Mutex<Vec<PortalEvent>>>,
}

impl PortalWatch {
    /// Opens its own `Connection::session()`, calls `BecomeMonitor` with the two match rules,
    /// and reads on a thread until the bus ends. Refuses with `IsolationBreach` before
    /// connecting if a desktop socket is reachable; other errors are `Internal`.
    pub fn start() -> Result<PortalWatch, SessionError> {
        start_guarded(&desktop_addresses(rustix::process::getuid().as_raw()))
    }

    /// A copy of every event so far.
    pub fn events(&self) -> Vec<PortalEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// `PortalWatch::start` behind the negative connect check on `desktop`: outside the sandbox the
/// session bus would be the user's own, which must never be monitored.
fn start_guarded(desktop: &[Address]) -> Result<PortalWatch, SessionError> {
    check(desktop)?;
    // BecomeMonitor turns its connection receive-only, so the watch never shares one.
    let connection = Connection::session()?;
    let messages = MessageIterator::from(&connection);
    let rules = RULES
        .iter()
        .map(|rule| MatchRule::try_from(*rule))
        .collect::<Result<Vec<_>, _>>()?;
    MonitoringProxy::new(&connection)?
        .become_monitor(&rules, 0)
        .map_err(|e| SessionError::Internal(format!("BecomeMonitor: {e}")))?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&events);
    std::thread::spawn(move || {
        let _connection = connection;
        for message in messages.map_while(Result::ok) {
            if let Some(event) = event_from(&message) {
                log.lock().unwrap_or_else(|e| e.into_inner()).push(event);
            }
        }
    });
    Ok(PortalWatch { events })
}

/// A monitored message as an event; bodies that do not parse are dropped.
fn event_from(message: &zbus::Message) -> Option<PortalEvent> {
    let header = message.header();
    match header.message_type() {
        Type::MethodCall => {
            let member = header.member()?;
            let (_, _, options): (String, String, HashMap<String, OwnedValue>) =
                message.body().deserialize().ok()?;
            prompt_from_call(member.as_str(), &options).map(PortalEvent::Prompt)
        }
        Type::Signal => {
            let (code, results): (u32, HashMap<String, OwnedValue>) =
                message.body().deserialize().ok()?;
            Some(response_from_signal(code, &results))
        }
        _ => None,
    }
}

/// `OpenFile` or `SaveFile` options as a prompt (`current_folder` without its trailing NUL as
/// `directory`, `current_name` as `suggested_name`, `multiple`); any other member is `None`.
pub fn prompt_from_call(member: &str, options: &HashMap<String, OwnedValue>) -> Option<PromptInfo> {
    let kind = match member {
        "OpenFile" => PromptKind::Open,
        "SaveFile" => PromptKind::Save,
        _ => return None,
    };
    let multiple = options
        .get("multiple")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false);
    let mut prompt = PromptInfo::new(kind, multiple);
    // current_folder is a NUL-terminated byte array, not a string (spike §5).
    prompt.directory = options
        .get("current_folder")
        .and_then(|v| Vec::<u8>::try_from(v.try_clone().ok()?).ok())
        .map(|mut bytes| {
            if bytes.last() == Some(&0) {
                bytes.pop();
            }
            PathBuf::from(OsString::from_vec(bytes))
        });
    prompt.suggested_name = options
        .get("current_name")
        .and_then(|v| <&str>::try_from(v).ok())
        .map(str::to_owned);
    Some(prompt)
}

/// A `Response` signal as an event; missing `uris` is empty.
pub fn response_from_signal(code: u32, results: &HashMap<String, OwnedValue>) -> PortalEvent {
    let uris = results
        .get("uris")
        .and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default();
    PortalEvent::Response { code, uris }
}

/// The last prompt with no response after it, and its index.
pub fn pending(events: &[PortalEvent]) -> Option<(usize, PromptInfo)> {
    match events.iter().enumerate().next_back()? {
        (index, PortalEvent::Prompt(prompt)) => Some((index, prompt.clone())),
        _ => None,
    }
}

/// The first response after the prompt at `index`.
pub fn response_after(events: &[PortalEvent], index: usize) -> Option<(u32, Vec<String>)> {
    events.iter().skip(index + 1).find_map(|event| match event {
        PortalEvent::Response { code, uris } => Some((*code, uris.clone())),
        _ => None,
    })
}

/// A `file://` URI's percent-decoded path; any other scheme or a bad escape is `None`.
pub fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    if !encoded.starts_with('/') {
        return None;
    }
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut rest = encoded.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    if bytes.len() > 1 && bytes.last() == Some(&b'/') {
        bytes.pop();
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

/// The text typed into the dialog: one path verbatim, several as `"a" "b"`. Empty, non-UTF-8,
/// or `"` inside a path of several is `InvalidArgument`.
pub fn dialog_text(paths: &[PathBuf]) -> Result<String, SessionError> {
    let invalid = SessionError::InvalidArgument;
    match paths {
        [] => Err(invalid("no paths to answer with".into())),
        [one] => utf8(one).map(str::to_owned),
        // KDE's location edit splits several names on quotes, so a quote cannot be escaped.
        several => {
            let mut quoted = Vec::with_capacity(several.len());
            for path in several {
                let path = utf8(path)?;
                if path.contains('"') {
                    return Err(invalid(format!(
                        "{path:?} holds a quote, which a multiple answer cannot type"
                    )));
                }
                quoted.push(format!("\"{path}\""));
            }
            Ok(quoted.join(" "))
        }
    }
}

/// A path as the UTF-8 text EIS can type.
fn utf8(path: &Path) -> Result<&str, SessionError> {
    path.to_str()
        .ok_or_else(|| SessionError::InvalidArgument(format!("{} is not UTF-8", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_pilot_protocol::{ErrorCode, PromptKind};
    use zbus::zvariant::Value;

    fn opts(pairs: Vec<(&str, Value<'_>)>) -> HashMap<String, OwnedValue> {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.try_to_owned().unwrap()))
            .collect()
    }

    #[test]
    fn a_reachable_desktop_socket_refuses_the_watch_before_any_bus_connect() {
        let dir = tempfile::Builder::new()
            .prefix("gp-")
            .tempdir_in("/tmp")
            .unwrap();
        let bus = dir.path().join("bus");
        let _listener = std::os::unix::net::UnixListener::bind(&bus).unwrap();
        let error = start_guarded(&[Address::Path(bus)])
            .err()
            .expect("a breach");
        assert_eq!(error.code(), ErrorCode::IsolationBreach, "{error}");
    }

    #[test]
    fn open_and_save_calls_become_prompts() {
        let open = prompt_from_call(
            "OpenFile",
            &opts(vec![
                ("multiple", Value::from(true)),
                ("directory", Value::from(false)),
                ("handle_token", Value::from("ashpd_x")),
            ]),
        )
        .unwrap();
        assert_eq!((open.kind, open.multiple), (PromptKind::Open, true));
        assert_eq!(
            (open.directory.clone(), open.suggested_name.clone()),
            (None, None)
        );
        let save = prompt_from_call(
            "SaveFile",
            &opts(vec![
                ("current_folder", Value::from(b"/tmp\0".to_vec())),
                ("current_name", Value::from("demo.txt")),
            ]),
        )
        .unwrap();
        assert_eq!((save.kind, save.multiple), (PromptKind::Save, false));
        assert_eq!(save.directory, Some(PathBuf::from("/tmp")));
        assert_eq!(save.suggested_name.as_deref(), Some("demo.txt"));
        assert_eq!(prompt_from_call("SaveFiles", &opts(vec![])), None);
    }

    #[test]
    fn responses_carry_their_code_and_uris() {
        let ok = response_from_signal(
            0,
            &opts(vec![("uris", Value::from(vec!["file:///tmp/a%20b.txt"]))]),
        );
        assert_eq!(
            ok,
            PortalEvent::Response {
                code: 0,
                uris: vec!["file:///tmp/a%20b.txt".into()]
            }
        );
        assert_eq!(
            response_from_signal(1, &opts(vec![])),
            PortalEvent::Response {
                code: 1,
                uris: vec![]
            }
        );
    }

    #[test]
    fn the_pending_prompt_is_the_last_one_without_a_response() {
        let open = PromptInfo::new(PromptKind::Open, false);
        let save = PromptInfo::new(PromptKind::Save, false);
        let events = vec![
            PortalEvent::Prompt(open),
            PortalEvent::Response {
                code: 1,
                uris: vec![],
            },
            PortalEvent::Prompt(save.clone()),
        ];
        assert_eq!(pending(&[]), None);
        assert_eq!(pending(&events), Some((2, save)));
        assert_eq!(pending(&events[..2]), None);
        assert_eq!(response_after(&events, 0), Some((1, vec![])));
        assert_eq!(response_after(&events, 2), None);
    }

    #[test]
    fn file_uris_decode_to_paths() {
        assert_eq!(
            file_uri_path("file:///tmp/gp-X/pick/in/a.txt"),
            Some(PathBuf::from("/tmp/gp-X/pick/in/a.txt"))
        );
        assert_eq!(
            file_uri_path("file:///tmp/with%20space.txt"),
            Some(PathBuf::from("/tmp/with space.txt"))
        );
        assert_eq!(
            file_uri_path("file:///tmp/%E6%BC%A2.txt"),
            Some(PathBuf::from("/tmp/漢.txt"))
        );
        assert_eq!(
            file_uri_path("file:///tmp/dir/"),
            Some(PathBuf::from("/tmp/dir"))
        );
        assert_eq!(file_uri_path("https://example.org/a"), None);
        assert_eq!(file_uri_path("file:///bad%zz"), None);
    }

    #[test]
    fn one_path_is_typed_verbatim_and_several_are_quoted() {
        assert_eq!(
            dialog_text(&[PathBuf::from("/tmp/with space.txt")]).unwrap(),
            "/tmp/with space.txt"
        );
        assert_eq!(
            dialog_text(&[PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b c.txt")]).unwrap(),
            "\"/tmp/a.txt\" \"/tmp/b c.txt\""
        );
        let quote =
            dialog_text(&[PathBuf::from("/tmp/a\".txt"), PathBuf::from("/tmp/b.txt")]).unwrap_err();
        assert!(matches!(quote, SessionError::InvalidArgument(_)), "{quote}");
        assert!(matches!(
            dialog_text(&[]),
            Err(SessionError::InvalidArgument(_))
        ));
        assert_eq!(
            SessionError::InvalidArgument("x".into()).code(),
            ErrorCode::InvalidArgument
        );
    }
}
