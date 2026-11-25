//! The app's AT-SPI tree, read from the session's accessibility bus as protocol `Node`s.

use std::time::{Duration, Instant};

use atspi_common::{CoordType, State, StateSet};
use atspi_proxies::accessible::AccessibleProxyBlocking;
use atspi_proxies::component::ComponentProxyBlocking;
use gpui_pilot_protocol::{Node, Rect, Segment};
use zbus::blocking::Connection;
use zbus::proxy::CacheProperties;

use crate::SessionError;

const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT: &str = "/org/a11y/atspi/accessible/root";
const UNKNOWN_OBJECT: &str = "org.freedesktop.DBus.Error.UnknownObject";
const NO_REPLY: &str = "org.freedesktop.DBus.Error.NoReply";
const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
const UNKNOWN_INTERFACE: &str = "org.freedesktop.DBus.Error.UnknownInterface";

/// How long one call to the app may take before it counts as not answering.
pub(crate) const CALL_LIMIT: Duration = Duration::from_secs(2);
/// The longest one tree walk may take.
pub(crate) const WALK_LIMIT: Duration = Duration::from_secs(10);

/// Walks every node below `pid`'s application object, sorted by top, left, then path.
/// `content` supplies the client area relative to the surface origin: its size bounds the frame,
/// which AT-SPI reports as `(-1, -1, -1, -1)`, and its origin moves extents into the client.
pub(crate) fn walk(
    a11y: &Connection,
    pid: u32,
    scale: f32,
    deadline: Instant,
    content: impl FnOnce() -> Result<Rect, SessionError>,
) -> Result<Vec<Node>, SessionError> {
    let (name, path) = application(a11y, pid)?;
    let found = collect(
        path,
        deadline,
        |path| {
            let children = accessible(a11y, &name, path)?.get_children()?;
            Ok(children
                .iter()
                .map(|c| c.path_as_str().to_owned())
                .collect())
        },
        |path, indices| read(a11y, &accessible(a11y, &name, path)?, &name, path, indices),
    )?;
    // AT-SPI counts from the wl_surface, which a client-side decoration's shadow pads beyond
    // the client area that points and captures use; one KWin script reads that inset.
    let content = content()?;
    let mut nodes: Vec<_> = found
        .into_iter()
        .map(|raw| (raw.indices.clone(), raw.into_node(scale, &content)))
        .collect();
    // Top, then left; the child-index path breaks ties so the order is stable.
    nodes.sort_by(|(a_path, a), (b_path, b)| {
        (a.bounds.y.total_cmp(&b.bounds.y))
            .then(a.bounds.x.total_cmp(&b.bounds.x))
            .then_with(|| a_path.cmp(b_path))
    });
    Ok(nodes.into_iter().map(|(_, node)| node).collect())
}

/// Depth-first from the application object at `root`, reading each node below it with its
/// child-index path; `children` lists a node's child object paths. A node below the root that
/// vanishes mid-walk (`UnknownObject`, as a re-rendering app drops it) is skipped with its
/// subtree; any other error, or the root vanishing, fails the walk. A call the app does not
/// answer fails it as `Unanswered`, a walk still going at `deadline` as `Timeout`.
fn collect(
    root: String,
    deadline: Instant,
    mut children: impl FnMut(&str) -> zbus::Result<Vec<String>>,
    mut read: impl FnMut(&str, Vec<u64>) -> zbus::Result<Raw>,
) -> Result<Vec<Raw>, SessionError> {
    let mut found = Vec::new();
    let mut stack = vec![(root, Vec::<u64>::new())];
    while let Some((path, indices)) = stack.pop() {
        if Instant::now() >= deadline {
            return Err(SessionError::Timeout(format!(
                "reading the accessibility tree passed its deadline after {} nodes",
                found.len()
            )));
        }
        let below_root = !indices.is_empty();
        // The application object itself is not a node; only what lies below it is read.
        let node = below_root.then(|| read(&path, indices.clone())).transpose();
        let listed = node.and_then(|node| Ok((node, children(&path)?)));
        let (node, kids) = match listed {
            Ok(listed) => listed,
            Err(error) if not_answering(&error) => {
                return Err(SessionError::Unanswered(format!(
                    "the app did not answer an accessibility request within {} s",
                    CALL_LIMIT.as_secs()
                )));
            }
            Err(error) if below_root && vanished(&error) => continue,
            Err(error) => return Err(error.into()),
        };
        found.extend(node);
        for (i, child) in kids.into_iter().enumerate() {
            let mut child_indices = indices.clone();
            child_indices.push(i as u64);
            stack.push((child, child_indices));
        }
    }
    Ok(found)
}

/// True for a call the peer did not answer in time: io `TimedOut`, or a `NoReply` error.
fn not_answering(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::InputOutput(e) => e.kind() == std::io::ErrorKind::TimedOut,
        zbus::Error::MethodError(name, ..) => name.as_str() == NO_REPLY,
        zbus::Error::FDO(error) => matches!(**error, zbus::fdo::Error::NoReply(_)),
        _ => false,
    }
}

/// `Ok(None)` when the object lacks the interface; every other error propagates.
fn without_interface<T>(result: zbus::Result<T>) -> zbus::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(zbus::Error::MethodError(name, ..))
            if matches!(name.as_str(), UNKNOWN_METHOD | UNKNOWN_INTERFACE) =>
        {
            Ok(None)
        }
        Err(zbus::Error::FDO(error))
            if matches!(
                *error,
                zbus::fdo::Error::UnknownMethod(_) | zbus::fdo::Error::UnknownInterface(_)
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// True when `error` says the object is no longer on the bus.
fn vanished(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, ..) => name.as_str() == UNKNOWN_OBJECT,
        zbus::Error::FDO(error) => matches!(**error, zbus::fdo::Error::UnknownObject(_)),
        _ => false,
    }
}

/// The application object (bus name and path) whose connection belongs to process `pid`.
fn application(a11y: &Connection, pid: u32) -> Result<(String, String), SessionError> {
    let root = accessible(a11y, REGISTRY, ROOT)?;
    for app in root.get_children()? {
        let Some(name) = app.name_as_str() else {
            continue;
        };
        // An app that left the bus since the registry listed it simply does not match.
        let owner = a11y
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetConnectionUnixProcessID",
                &(name,),
            )
            .and_then(|reply| reply.body().deserialize::<u32>());
        if owner.ok() == Some(pid) {
            return Ok((name.to_owned(), app.path_as_str().to_owned()));
        }
    }
    Err(SessionError::LaunchFailed {
        stage: "atspi",
        message: format!("no accessible application has pid {pid}"),
    })
}

fn accessible<'a>(
    a11y: &Connection,
    name: &'a str,
    path: &'a str,
) -> zbus::Result<AccessibleProxyBlocking<'a>> {
    AccessibleProxyBlocking::builder(a11y)
        .destination(name)?
        .path(path)?
        .cache_properties(CacheProperties::No)
        .build()
}

/// One node's attributes as AT-SPI reports them, before scaling.
#[derive(Debug)]
struct Raw {
    indices: Vec<u64>,
    id: String,
    role: String,
    name: String,
    extents: Option<(i32, i32, i32, i32)>,
    state: StateSet,
}

fn read(
    a11y: &Connection,
    accessible: &AccessibleProxyBlocking<'_>,
    name: &str,
    path: &str,
    indices: Vec<u64>,
) -> zbus::Result<Raw> {
    let component = ComponentProxyBlocking::builder(a11y)
        .destination(name)?
        .path(path)?
        .cache_properties(CacheProperties::No)
        .build()?;
    Ok(Raw {
        indices,
        id: accessible.accessible_id()?,
        role: format!("{:?}", accessible.get_role()?),
        name: accessible.name()?,
        // A node without the Component interface is treated like the frame; any other failure,
        // a timeout above all, fails the walk instead of letting it limp on.
        extents: without_interface(component.get_extents(CoordType::Window))?,
        state: accessible.get_state()?,
    })
}

impl Raw {
    /// False for the frame, which reports `(-1, -1, -1, -1)`, and for a node without the
    /// Component interface.
    fn has_extents(&self) -> bool {
        self.extents.is_some_and(|e| e != (-1, -1, -1, -1))
    }

    /// The protocol node in client coordinates: device-pixel extents divided by `scale` and
    /// shifted by `content`'s origin, or `(0, 0, w, h)` of `content` for a node without extents.
    fn into_node(self, scale: f32, content: &Rect) -> Node {
        let bounds = match self.extents.filter(|_| self.has_extents()) {
            Some((x, y, w, h)) => Rect::new(
                x as f32 / scale - content.x,
                y as f32 / scale - content.y,
                w as f32 / scale,
                h as f32 / scale,
            ),
            None => Rect::new(0., 0., content.w, content.h),
        };
        let has = |state: State| self.state.contains(state);
        let depth = self.indices.len() as u32 - 1;
        let path = self.indices.into_iter().map(Segment::Int).collect();
        let mut node = Node::new(path, bounds, has(State::Showing), depth);
        node.id = Some(self.id).filter(|s| !s.is_empty());
        node.role = Some(self.role);
        node.name = Some(self.name).filter(|s| !s.is_empty());
        node.focused = Some(has(State::Focused));
        node.disabled = (!has(State::Enabled)).then_some(true);
        node.checked = has(State::Checkable).then(|| has(State::Checked));
        node.selected = has(State::Selectable).then(|| has(State::Selected));
        node.expanded = has(State::Expandable).then(|| has(State::Expanded));
        node
    }
}

/// `None` when `after` reads as the same settled tree as `before`: equal `(path, id, role)`
/// sets, compared by path, and equal bounds for every node another node's path extends.
/// Otherwise the reason, quoted in `LaunchFailed`.
pub(crate) fn unstable(before: &[Node], after: &[Node]) -> Option<&'static str> {
    // Match by path, since the walk orders by position and a moving leaf reorders the list.
    // Only containers must hold still: leaves such as canvas shapes may animate forever.
    if before.len() != after.len() {
        return Some("the elements kept changing");
    }
    let has_children = |n: &Node| {
        after
            .iter()
            .any(|m| m.path.len() > n.path.len() && m.path.starts_with(&n.path))
    };
    for n in after {
        let Some(m) = before.iter().find(|m| m.path == n.path) else {
            return Some("the elements kept changing");
        };
        if m.id != n.id || m.role != n.role {
            return Some("the elements kept changing");
        }
        if m.bounds != n.bounds && has_children(n) {
            return Some("the bounds of an element with children kept changing");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn raw(extents: Option<(i32, i32, i32, i32)>, states: &[State]) -> Raw {
        Raw {
            indices: vec![0, 2],
            id: String::new(),
            role: "CheckBox".into(),
            name: "Agree".into(),
            extents,
            state: states.iter().collect(),
        }
    }

    #[test]
    fn extents_scale_down_and_states_map_only_when_applicable() {
        let raw = raw(Some((24, 75, 30, 48)), &[State::Showing, State::Checkable]);
        assert!(raw.has_extents());
        let node = raw.into_node(1.5, &Rect::new(0., 0., 1280., 764.));
        assert_eq!(node.bounds, Rect::new(16., 50., 20., 32.));
        assert_eq!((node.depth, node.path.len()), (1, 2));
        assert_eq!((node.id, node.name.as_deref()), (None, Some("Agree")));
        assert_eq!((node.focused, node.disabled), (Some(false), Some(true)));
        assert_eq!(
            (node.checked, node.selected, node.expanded),
            (Some(false), None, None)
        );
    }

    #[test]
    fn the_frame_takes_the_client_size() {
        let raw = raw(Some((-1, -1, -1, -1)), &[State::Enabled]);
        assert!(!raw.has_extents());
        let node = raw.into_node(1.5, &Rect::new(20., 20., 1280., 764.));
        assert_eq!(node.bounds, Rect::new(0., 0., 1280., 764.));
        assert!(!node.visible && node.disabled.is_none());
    }

    #[test]
    fn extents_move_from_the_surface_into_the_client() {
        // pilot-demo's `count` under client-side decorations: surface px, 20 px shadow inset.
        let raw = raw(Some((55, 106, 1869, 48)), &[State::Showing]);
        let node = raw.into_node(1.5, &Rect::new(20., 20., 1280., 800.));
        assert_eq!(
            node.bounds,
            Rect::new(55. / 1.5 - 20., 106. / 1.5 - 20., 1246., 32.)
        );
    }

    /// A fake accessible tree: `children` per path, and the paths whose objects are gone.
    fn fake_walk(
        children: &[(&str, &[&str])],
        gone: &[&str],
        fail: fn(&str) -> zbus::Error,
    ) -> Result<Vec<Vec<u64>>, SessionError> {
        let raws = collect(
            "app".into(),
            Instant::now() + Duration::from_secs(60),
            |path| {
                if gone.contains(&path) {
                    return Err(fail(path));
                }
                Ok(children
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, c)| c.iter().map(|s| s.to_string()).collect())
                    .unwrap_or_default())
            },
            |path, indices| {
                if gone.contains(&path) {
                    return Err(fail(path));
                }
                let mut raw = raw(None, &[]);
                raw.indices = indices;
                Ok(raw)
            },
        )?;
        let mut paths: Vec<_> = raws.into_iter().map(|r| r.indices).collect();
        paths.sort();
        Ok(paths)
    }

    /// The error reply a D-Bus peer sends, as zbus surfaces it from a proxy call.
    fn reply(name: &str, path: &str) -> zbus::Error {
        let call = zbus::Message::method_call("/org/a11y/atspi/accessible/0/1", "GetChildren")
            .unwrap()
            .build(&())
            .unwrap();
        let error = zbus::Message::error(&call.header(), name)
            .unwrap()
            .build(&(format!("Unknown object '{path}'"),))
            .unwrap();
        zbus::Error::from(error)
    }

    const TREE: &[(&str, &[&str])] = &[("app", &["a", "b"]), ("a", &["a1"]), ("b", &["b1"])];

    #[test]
    fn a_node_that_vanishes_mid_walk_drops_only_its_subtree() {
        let unknown = |p: &str| reply("org.freedesktop.DBus.Error.UnknownObject", p);
        let paths = fake_walk(TREE, &["a"], unknown).unwrap();
        assert_eq!(paths, vec![vec![1], vec![1, 0]]);
        let leaf = fake_walk(TREE, &["b1"], unknown).unwrap();
        assert_eq!(leaf, vec![vec![0], vec![0, 0], vec![1]]);
        let fdo = |p: &str| zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownObject(p.into())));
        assert_eq!(fake_walk(TREE, &["a"], fdo).unwrap(), paths);
    }

    #[test]
    fn a_vanished_application_or_another_error_still_fails_the_walk() {
        let unknown = |p: &str| reply("org.freedesktop.DBus.Error.UnknownObject", p);
        assert!(fake_walk(TREE, &["app"], unknown).is_err());
        let denied = |p: &str| reply("org.freedesktop.DBus.Error.AccessDenied", p);
        assert!(fake_walk(TREE, &["a"], denied).is_err());
    }

    /// `TREE`'s children of `path`, for walks that need no failures.
    fn kids(path: &str) -> zbus::Result<Vec<String>> {
        Ok(TREE
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, c)| c.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default())
    }

    fn timed_out(_: &str) -> zbus::Error {
        zbus::Error::from(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out",
        ))
    }

    #[test]
    fn a_call_the_app_does_not_answer_fails_the_walk_as_unanswered() {
        let no_reply = |p: &str| reply("org.freedesktop.DBus.Error.NoReply", p);
        let fdo = |p: &str| zbus::Error::FDO(Box::new(zbus::fdo::Error::NoReply(p.into())));
        let fails: [fn(&str) -> zbus::Error; 3] = [timed_out, no_reply, fdo];
        for fail in fails {
            // The application object, an inner node and a leaf: none is skipped as vanished.
            for gone in ["app", "a", "b1"] {
                match fake_walk(TREE, &[gone], fail) {
                    Err(SessionError::Unanswered(m)) => {
                        assert!(m.contains("did not answer") && m.contains("2 s"), "{m}");
                        assert!(!m.contains("optimised"), "no launch advice: {m}");
                    }
                    other => panic!("{gone}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn a_walk_past_its_deadline_fails_naming_the_nodes_read() {
        // Depth first from the last child: b, then b1, which outlasts the deadline before a.
        let started = Instant::now();
        let slow = collect(
            "app".into(),
            started + Duration::from_millis(100),
            kids,
            |path, indices| {
                if path == "b1" {
                    std::thread::sleep(Duration::from_millis(150));
                }
                let mut r = raw(None, &[]);
                r.indices = indices;
                Ok(r)
            },
        );
        match slow {
            Err(SessionError::Timeout(m)) => {
                assert!(m.contains("passed its deadline after 2 nodes"), "{m}")
            }
            other => panic!("{other:?}"),
        }
        let late = collect("app".into(), Instant::now(), kids, |_, _| {
            panic!("no node is read past the deadline")
        });
        match late {
            Err(SessionError::Timeout(m)) => assert!(m.contains("after 0 nodes"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_walk_within_its_deadline_reads_every_node() {
        let all = collect(
            "app".into(),
            Instant::now() + Duration::from_secs(60),
            kids,
            |_, indices| {
                let mut r = raw(None, &[]);
                r.indices = indices;
                Ok(r)
            },
        )
        .unwrap();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn only_a_missing_interface_reads_as_no_extents() {
        let extents = (1, 2, 3, 4);
        assert_eq!(without_interface(Ok(extents)).unwrap(), Some(extents));
        for name in [
            "org.freedesktop.DBus.Error.UnknownMethod",
            "org.freedesktop.DBus.Error.UnknownInterface",
        ] {
            let missing = without_interface::<(i32, i32, i32, i32)>(Err(reply(name, "a")));
            assert_eq!(missing.unwrap(), None, "{name}");
        }
        let fdo = zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownInterface("a".into())));
        assert_eq!(without_interface::<()>(Err(fdo)).unwrap(), None);
        assert!(without_interface::<()>(Err(timed_out("a"))).is_err());
        let unknown = reply("org.freedesktop.DBus.Error.UnknownObject", "a");
        assert!(
            without_interface::<()>(Err(unknown)).is_err(),
            "a vanished node is not a missing interface"
        );
    }

    fn n(path: &[u64], id: Option<&str>, role: &str, bounds: Rect) -> Node {
        let segments = path.iter().map(|i| Segment::Int(*i)).collect();
        let mut node = Node::new(segments, bounds, true, path.len() as u32 - 1);
        node.id = id.map(str::to_owned);
        node.role = Some(role.to_owned());
        node
    }

    /// A frame holding a leaf button and a board with two shapes, the shapes moved by `pan`.
    fn board(pan: f32) -> Vec<Node> {
        vec![
            n(&[0], None, "Frame", Rect::new(0., 0., 1280., 764.)),
            n(&[0, 0], None, "Button", Rect::new(16., 16., 1248., 32.)),
            n(&[0, 1], None, "Canvas", Rect::new(16., 56., 600., 300.)),
            n(
                &[0, 1, 0],
                Some("shape-0"),
                "Panel",
                Rect::new(-64. + pan, 71., 160., 120.),
            ),
            n(
                &[0, 1, 1],
                Some("shape-1"),
                "Panel",
                Rect::new(236. + pan, 71., 160., 120.),
            ),
        ]
    }

    #[test]
    fn equal_trees_are_stable_in_any_order() {
        let mut after = board(0.);
        after.reverse();
        assert_eq!(unstable(&board(0.), &after), None);
    }

    #[test]
    fn moving_leaves_do_not_unsettle_the_tree() {
        // A pan moves the shapes and reorders the walk's top-then-left list.
        let mut after = board(330.);
        after.swap(3, 4);
        assert_eq!(unstable(&board(0.), &after), None);
        let mut button = board(0.);
        button[1].bounds.w = 1200.;
        assert_eq!(unstable(&board(0.), &button), None, "the button is a leaf");
    }

    #[test]
    fn a_moving_container_unsettles_the_tree_and_says_so() {
        let mut after = board(0.);
        after[2].bounds.x += 1.;
        assert_eq!(
            unstable(&board(0.), &after),
            Some("the bounds of an element with children kept changing")
        );
        let mut frame = board(0.);
        frame[0].bounds.w = 1200.;
        assert!(
            unstable(&board(0.), &frame).is_some(),
            "the frame holds every node"
        );
    }

    #[test]
    fn a_changed_element_unsettles_the_tree_and_says_so() {
        let mut gone = board(0.);
        gone.pop();
        assert_eq!(
            unstable(&board(0.), &gone),
            Some("the elements kept changing")
        );
        let mut renamed = board(0.);
        renamed[4].id = Some("shape-9".into());
        assert_eq!(
            unstable(&board(0.), &renamed),
            Some("the elements kept changing")
        );
        let mut roled = board(0.);
        roled[1].role = Some("Label".into());
        assert_eq!(
            unstable(&board(0.), &roled),
            Some("the elements kept changing")
        );
    }
}
