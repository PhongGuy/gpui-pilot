//! The app's AT-SPI tree, read from the session's accessibility bus as protocol `Node`s.

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

/// Walks every node below `pid`'s application object, sorted by top, left, then path.
/// `content` supplies the client area relative to the surface origin: its size bounds the frame,
/// which AT-SPI reports as `(-1, -1, -1, -1)`, and its origin moves extents into the client.
pub(crate) fn walk(
    a11y: &Connection,
    pid: u32,
    scale: f32,
    content: impl FnOnce() -> Result<Rect, SessionError>,
) -> Result<Vec<Node>, SessionError> {
    let (name, path) = application(a11y, pid)?;
    let found = collect(
        path,
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
/// subtree; any other error, or the root vanishing, fails the walk.
fn collect(
    root: String,
    mut children: impl FnMut(&str) -> zbus::Result<Vec<String>>,
    mut read: impl FnMut(&str, Vec<u64>) -> zbus::Result<Raw>,
) -> Result<Vec<Raw>, SessionError> {
    let mut found = Vec::new();
    let mut stack = vec![(root, Vec::<u64>::new())];
    while let Some((path, indices)) = stack.pop() {
        let below_root = !indices.is_empty();
        // The application object itself is not a node; only what lies below it is read.
        let node = below_root.then(|| read(&path, indices.clone())).transpose();
        let listed = node.and_then(|node| Ok((node, children(&path)?)));
        let (node, kids) = match listed {
            Ok(listed) => listed,
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
        // A node without the Component interface fails here; it is treated like the frame.
        extents: component.get_extents(CoordType::Window).ok(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
