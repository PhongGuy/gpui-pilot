//! Target resolution and actionability over the session's AT-SPI nodes. Error messages mirror
//! the headless host's so agents see the same text in both modes.

use gpui_pilot_protocol::{ErrorCode, Node, PilotError, Rect, Segment, Target};

/// A resolved element ready for input, and the point to act at.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub struct Actionable {
    pub node: Node,
    /// Window-content logical px.
    pub at: (f32, f32),
}

/// The node an element target names. A one-string id matches `Node.id` exactly and a
/// non-empty `id_suffix` (else `InvalidArgument`) matches its end; role and exact name
/// match with an optional `nth` (0-based, in node order). Zero matches is `ElementNotFound`
/// (with up to five `nearest` ids), several without `nth` `Ambiguous` (with `matches`), an
/// `nth` past the end `ElementNotFound` naming the index; a point target is `Unsupported`;
/// any other id shape is `InvalidArgument`.
pub fn resolve(nodes: &[Node], target: &Target) -> Result<Node, PilotError> {
    match target {
        Target::Id { id } => {
            let [Segment::Str(wanted)] = id.as_slice() else {
                return Err(PilotError::new(
                    ErrorCode::InvalidArgument,
                    "a session id is one string segment holding the element's accessible id",
                ));
            };
            let found: Vec<_> = nodes
                .iter()
                .filter(|n| n.id.as_deref() == Some(wanted.as_str()))
                .collect();
            pick(found, None, || {
                let mut error = PilotError::new(
                    ErrorCode::ElementNotFound,
                    format!("no element path matches [{wanted:?}]"),
                );
                error.nearest = nearest(nodes, wanted);
                error
            })
        }
        Target::IdSuffix { id_suffix } => {
            if id_suffix.is_empty() {
                return Err(PilotError::new(
                    ErrorCode::InvalidArgument,
                    "`id_suffix` must not be empty",
                ));
            }
            let found: Vec<_> = nodes
                .iter()
                .filter(|n| n.id.as_deref().is_some_and(|id| id.ends_with(id_suffix)))
                .collect();
            pick(found, None, || {
                let mut error = PilotError::new(
                    ErrorCode::ElementNotFound,
                    format!("no element's own id ends with {id_suffix:?}"),
                );
                error.nearest = nearest(nodes, id_suffix);
                error
            })
        }
        Target::Role { role, name, nth } => {
            let found: Vec<_> = nodes
                .iter()
                .filter(|n| {
                    n.role
                        .as_deref()
                        .is_some_and(|r| r.eq_ignore_ascii_case(role))
                        && n.name.as_deref() == Some(name.as_str())
                })
                .collect();
            pick(found, *nth, || {
                PilotError::new(
                    ErrorCode::ElementNotFound,
                    format!("no {role} element is named {name:?}"),
                )
            })
        }
        _ => Err(PilotError::new(
            ErrorCode::Unsupported,
            "a point target names no element",
        )),
    }
}

/// A point target acts at the point without reading the tree, so a re-rendering app cannot
/// fail it; its node is pathless with zero-size bounds at the point. `None` for any other target.
pub fn at_point(target: &Target) -> Option<Actionable> {
    let Target::Point { x, y } = *target else {
        return None;
    };
    let node = Node::new(Vec::new(), Rect::new(x, y, 0., 0.), true, 0);
    Some(Actionable { node, at: (x, y) })
}

/// `resolve`, then: a hidden node is `ElementNotFound` ("the target matches only a hidden
/// element"), and a disabled one is `Disabled` when `need_enabled`; `at` is the bounds centre.
/// A point target goes through `at_point` instead.
pub fn actionable(
    nodes: &[Node],
    target: &Target,
    need_enabled: bool,
) -> Result<Actionable, PilotError> {
    let node = resolve(nodes, target)?;
    if !node.visible {
        return Err(PilotError::new(
            ErrorCode::ElementNotFound,
            "the target matches only a hidden element",
        ));
    }
    if need_enabled && node.disabled == Some(true) {
        return Err(PilotError::new(
            ErrorCode::Disabled,
            "the target element is disabled",
        ));
    }
    let Rect { x, y, w, h, .. } = node.bounds;
    Ok(Actionable {
        at: (x + w / 2.0, y + h / 2.0),
        node,
    })
}

/// The nodes whose path starts with `root.path`, `root` included, in input order.
pub fn within(nodes: &[Node], root: &Node) -> Vec<Node> {
    nodes
        .iter()
        .filter(|n| n.path.starts_with(&root.path))
        .cloned()
        .collect()
}

/// The one match, or the `nth` when given; `missing` builds the error for an empty choice.
fn pick(
    found: Vec<&Node>,
    nth: Option<u32>,
    missing: impl FnOnce() -> PilotError,
) -> Result<Node, PilotError> {
    match (nth, found.as_slice()) {
        (Some(n), _) => match found.get(n as usize) {
            Some(one) => Ok((*one).clone()),
            None => {
                let mut error = missing();
                error.message = format!("{} (index {n} of {} matches)", error.message, found.len());
                Err(error)
            }
        },
        (None, []) => Err(missing()),
        (None, [one]) => Ok((*one).clone()),
        (None, many) => {
            let mut error = PilotError::new(
                ErrorCode::Ambiguous,
                format!("{} elements match; narrow the target", many.len()),
            );
            error.matches = many.iter().map(|n| (*n).clone()).collect();
            Err(error)
        }
    }
}

/// Up to five ids to suggest for a miss: those overlapping the wanted text first, then the
/// rest, each group in node order.
fn nearest(nodes: &[Node], wanted: &str) -> Vec<Vec<Segment>> {
    let ids: Vec<&str> = nodes.iter().filter_map(|n| n.id.as_deref()).collect();
    let close = |id: &&str| id.contains(wanted) || wanted.contains(*id);
    let (first, rest): (Vec<&str>, Vec<&str>) = ids.into_iter().partition(close);
    first
        .into_iter()
        .chain(rest)
        .take(5)
        .map(|id| vec![Segment::Str(id.to_owned())])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_pilot_protocol::{ErrorCode, Rect, Segment, Target};

    fn node(path: &[u64], id: Option<&str>, role: &str, name: &str, bounds: Rect) -> Node {
        let segments = path.iter().map(|i| Segment::Int(*i)).collect::<Vec<_>>();
        let mut node = Node::new(segments, bounds, true, path.len() as u32 - 1);
        node.id = id.map(str::to_owned);
        node.role = Some(role.to_owned());
        node.name = (!name.is_empty()).then(|| name.to_owned());
        node
    }

    fn nodes() -> Vec<Node> {
        vec![
            node(&[0], None, "Frame", "", Rect::new(0., 0., 1280., 764.)),
            node(
                &[0, 0],
                Some("count"),
                "Button",
                "Count",
                Rect::new(16., 50., 1248., 32.),
            ),
            node(
                &[0, 1],
                Some("name"),
                "Entry",
                "",
                Rect::new(16., 90., 1248., 32.),
            ),
            node(
                &[0, 2],
                None,
                "Button",
                "Open",
                Rect::new(16., 130., 58., 32.),
            ),
            node(
                &[0, 3],
                None,
                "Button",
                "Open",
                Rect::new(82., 130., 58., 32.),
            ),
            node(
                &[0, 4],
                Some("counter-label"),
                "Label",
                "clicks",
                Rect::new(16., 10., 80., 20.),
            ),
        ]
    }

    fn id(text: &str) -> Target {
        Target::Id {
            id: vec![Segment::Str(text.to_owned())],
        }
    }

    fn role(role: &str, name: &str, nth: Option<u32>) -> Target {
        Target::Role {
            role: role.to_owned(),
            name: name.to_owned(),
            nth,
        }
    }

    #[test]
    fn ids_and_roles_resolve_to_their_node() {
        let all = nodes();
        assert_eq!(
            resolve(&all, &id("count")).unwrap().path,
            vec![Segment::Int(0), Segment::Int(0)]
        );
        assert_eq!(
            resolve(&all, &role("Entry", "", None)).unwrap_err().code,
            ErrorCode::ElementNotFound,
            "an empty name matches only a node named \"\"; unnamed nodes have no name"
        );
        assert_eq!(
            resolve(&all, &role("Button", "Count", None))
                .unwrap()
                .id
                .as_deref(),
            Some("count")
        );
        let second = resolve(&all, &role("Button", "Open", Some(1))).unwrap();
        assert_eq!(second.bounds.x, 82.);
    }

    #[test]
    fn misses_ambiguity_and_bad_ids_have_their_codes() {
        let all = nodes();
        let missing = resolve(&all, &id("coun")).unwrap_err();
        assert_eq!(missing.code, ErrorCode::ElementNotFound);
        assert!(
            missing
                .nearest
                .contains(&vec![Segment::Str("count".into())]),
            "{missing:?}"
        );
        assert!(missing.nearest.len() <= 5);
        let many = resolve(&all, &role("Button", "Open", None)).unwrap_err();
        assert_eq!(many.code, ErrorCode::Ambiguous);
        assert_eq!(many.matches.len(), 2);
        let past = resolve(&all, &role("Button", "Open", Some(2))).unwrap_err();
        assert_eq!(past.code, ErrorCode::ElementNotFound);
        assert!(past.message.contains("index 2 of 2"), "{}", past.message);
        let two = Target::Id {
            id: vec![Segment::Str("a".into()), Segment::Str("count".into())],
        };
        assert_eq!(
            resolve(&all, &two).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let int = Target::Id {
            id: vec![Segment::Int(3)],
        };
        assert_eq!(
            resolve(&all, &int).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let point = Target::Point { x: 1., y: 1. };
        assert_eq!(
            resolve(&all, &point).unwrap_err().code,
            ErrorCode::Unsupported
        );
    }

    #[test]
    fn an_id_suffix_matches_the_end_of_an_accessible_id() {
        // The shared fixture has no two ids with a common suffix; add one here.
        let mut all = nodes();
        all.push(node(
            &[0, 5],
            Some("re-count"),
            "Button",
            "Again",
            Rect::new(16., 170., 58., 32.),
        ));
        let suffix = |text: &str| Target::IdSuffix {
            id_suffix: text.to_owned(),
        };
        assert_eq!(
            resolve(&all, &suffix("-label")).unwrap().id.as_deref(),
            Some("counter-label")
        );
        assert_eq!(
            resolve(&all, &suffix("count")).unwrap_err().code,
            ErrorCode::Ambiguous
        );
        assert_eq!(
            resolve(&all, &suffix("zzz")).unwrap_err().code,
            ErrorCode::ElementNotFound
        );
        assert_eq!(
            resolve(&all, &suffix("")).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn actionable_checks_visibility_and_enabled_and_centres() {
        let mut all = nodes();
        let ready = actionable(&all, &id("count"), true).unwrap();
        assert_eq!(ready.at, (640., 66.));
        all[1].disabled = Some(true);
        assert_eq!(
            actionable(&all, &id("count"), true).unwrap_err().code,
            ErrorCode::Disabled
        );
        assert!(
            actionable(&all, &id("count"), false).is_ok(),
            "hover and scroll ignore enabled"
        );
        all[1].visible = false;
        let hidden = actionable(&all, &id("count"), false).unwrap_err();
        assert_eq!(hidden.code, ErrorCode::ElementNotFound);
        assert!(hidden.message.contains("hidden"), "{}", hidden.message);
    }

    #[test]
    fn a_point_acts_at_the_point_without_reading_the_tree() {
        // No node list is passed: a point target must not need the AT-SPI walk.
        let hit = at_point(&Target::Point { x: 20., y: 60. }).unwrap();
        assert_eq!(hit.at, (20., 60.));
        assert!(hit.node.path.is_empty() && hit.node.visible);
        assert_eq!(hit.node.bounds, Rect::new(20., 60., 0., 0.));
        assert!(
            at_point(&id("count")).is_none(),
            "an element target needs the tree"
        );
    }

    #[test]
    fn within_keeps_the_root_and_its_descendants() {
        let all = nodes();
        let frame = all[0].clone();
        assert_eq!(within(&all, &frame).len(), all.len());
        let count = all[1].clone();
        assert_eq!(within(&all, &count), vec![count]);
    }
}
