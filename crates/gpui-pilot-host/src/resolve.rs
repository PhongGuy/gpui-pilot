//! Target resolution against gpui-base's element snapshots, and their conversion to protocol
//! nodes. Only the non-asserting snapshot getters are read; kit lookups would panic.

use std::cmp::{Ordering, Reverse};
use std::collections::HashMap;

use crate::layers::Layer;
use gpui_kit::base::test_support::{ElementSnapshot, snapshots};
use gpui_kit::{Bounds, ElementId, Pixels, Point, Role, Window, point, px};
use gpui_pilot_protocol::{ErrorCode, Node, PilotError, Rect, Segment, Target};

// `snapshots` and its getters are doc-hidden, so bind their signatures here: a gpui-kit bump
// that changes them fails this build instead of the agent's first lookup.
const _: fn(&Window) -> Vec<ElementSnapshot> = snapshots;
const _: fn(&ElementSnapshot) -> &[ElementId] = ElementSnapshot::path;
const _: fn(&ElementSnapshot) -> Option<Role> = ElementSnapshot::role;
const _: fn(&ElementSnapshot) -> Option<&str> = ElementSnapshot::label;
const _: fn(&ElementSnapshot) -> Option<&str> = ElementSnapshot::value;
const _: fn(&ElementSnapshot) -> Bounds<Pixels> = ElementSnapshot::bounds;
const _: fn(&ElementSnapshot) -> bool = ElementSnapshot::visible;
const _: fn(&ElementSnapshot) -> Option<bool> = ElementSnapshot::checked;
const _: fn(&ElementSnapshot) -> Option<bool> = ElementSnapshot::selected;
const _: fn(&ElementSnapshot) -> Option<bool> = ElementSnapshot::expanded;
const _: fn(&ElementSnapshot) -> Option<bool> = ElementSnapshot::disabled;

/// How many near misses an `ElementNotFound` lists.
const NEAREST: usize = 5;

/// One registered element as the pilot observed it, in logical pixels.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Observed {
    pub path: Vec<ElementId>,
    pub role: Option<Role>,
    pub label: Option<String>,
    pub value: Option<String>,
    pub bounds: Bounds<Pixels>,
    pub visible: bool,
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub disabled: Option<bool>,
    /// Paint order, set only while the executor resolves a target against a tree.
    pub layer: Option<Layer>,
}

impl Observed {
    /// Visible, with every optional fact absent.
    pub fn new(path: Vec<ElementId>, bounds: Bounds<Pixels>) -> Self {
        Self {
            path,
            role: None,
            label: None,
            value: None,
            bounds,
            visible: true,
            checked: None,
            selected: None,
            expanded: None,
            disabled: None,
            layer: None,
        }
    }
}

/// gpui-base's `EDITOR_ELEMENT_ID` (crate-private): the last segment of a kit Input's editor.
const EDITOR_ELEMENT_ID: &str = "input-state";

/// The window's snapshots, converted without calling any asserting getter.
///
/// A kit Input's id names a role-less frame that declares its editor, at `frame/View/input-state`,
/// as semantic owner. The frame is reported as gpui-base's `find` reports it: its own path and
/// bounds, with the editor's semantics. The editor keeps its own entry too.
pub fn observe(window: &Window) -> Vec<Observed> {
    let snapshots = snapshots(window);
    let editors: HashMap<&[ElementId], &ElementSnapshot> = snapshots
        .iter()
        .filter_map(|snapshot| {
            let path = snapshot.path();
            match path {
                [frame @ .., ElementId::View(_), ElementId::Name(last)]
                    if last.as_ref() == EDITOR_ELEMENT_ID =>
                {
                    Some((frame, snapshot))
                }
                _ => None,
            }
        })
        .collect();
    snapshots
        .iter()
        .map(|snapshot| {
            let semantics = editors
                .get(snapshot.path())
                .filter(|_| snapshot.role().is_none())
                .copied()
                .unwrap_or(snapshot);
            Observed {
                path: snapshot.path().to_vec(),
                role: semantics.role(),
                label: semantics.label().map(str::to_owned),
                value: semantics.value().map(str::to_owned),
                bounds: snapshot.bounds(),
                visible: snapshot.visible(),
                checked: semantics.checked(),
                selected: semantics.selected(),
                expanded: semantics.expanded(),
                disabled: semantics.disabled(),
                layer: None,
            }
        })
        .collect()
}

/// Whether a text input holding the keyboard focus hides its text: a password input, or a
/// text input with no value, as gpui-component's test-support build reports a masked one.
pub fn focused_input_masked(window: &Window) -> bool {
    snapshots(window).iter().any(|snapshot| {
        let hides = match snapshot.role() {
            Some(Role::PasswordInput) => true,
            Some(
                Role::TextInput
                | Role::MultilineTextInput
                | Role::SearchInput
                | Role::EmailInput
                | Role::NumberInput
                | Role::PhoneNumberInput
                | Role::UrlInput,
            ) => snapshot.value().is_none(),
            _ => false,
        };
        hides && holds_focus(snapshot)
    })
}

/// Focus read from the snapshot's `Debug` instead of `focused()`, whose assert would end the
/// app on a missed binding; a changed format reads as unfocused, turning off only auto-masking.
fn holds_focus(snapshot: &ElementSnapshot) -> bool {
    debug_says_focused(&format!("{snapshot:?}"))
}

fn debug_says_focused(debug: &str) -> bool {
    debug.contains("focused: Some(true)")
}

/// The protocol segment for one id: integers stay integers, every other id is its `segment_text`.
pub fn segment_of(id: &ElementId) -> Segment {
    match id {
        ElementId::Integer(n) => Segment::Int(*n),
        other => Segment::Str(segment_text(other)),
    }
}

/// An id's display text with every code location shortened by `short_location`; other ids
/// display as GPUI shows them, except a named child (see `id_text`).
pub fn segment_text(id: &ElementId) -> String {
    id_text(id, true)
}

/// GPUI joins a named child as `inner-name`, so a hyphen in the name collides with a hyphen in
/// the inner id; such a name is bracketed instead (`connection[toggle-lite]`). A name without a
/// hyphen keeps GPUI's form. `short` shortens code locations.
fn id_text(id: &ElementId, short: bool) -> String {
    match id {
        ElementId::CodeLocation(l) if short => short_location(l.file(), l.line(), l.column()),
        ElementId::NamedChild(inner, name) if name.contains('-') => {
            format!("{}[{name}]", id_text(inner, short))
        }
        ElementId::NamedChild(inner, name) => format!("{}-{name}", id_text(inner, short)),
        other => other.to_string(),
    }
}

/// An absolute source path keeps only its crate directory onwards (the normal component before
/// the last `src`), or just its file name without one; a relative path stays whole.
fn short_location(file: &str, line: u32, column: u32) -> String {
    use std::path::{Component, Path};
    let path = Path::new(file);
    let shown = if path.is_absolute() {
        let parts: Vec<_> = path.components().collect();
        let crate_dir = (1..parts.len()).rev().find(|&i| {
            parts[i] == Component::Normal("src".as_ref())
                && matches!(parts[i - 1], Component::Normal(_))
        });
        match crate_dir {
            Some(i) => parts[i - 1..]
                .iter()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
            None => path.file_name().map_or_else(
                || file.to_owned(),
                |name| name.to_string_lossy().into_owned(),
            ),
        }
    } else {
        file.to_owned()
    };
    format!("{shown}:{line}:{column}")
}

/// An integer segment matches only an integer id; a text segment matches an id whose short or
/// full display text is equal to it.
pub fn segment_matches(segment: &Segment, id: &ElementId) -> bool {
    match segment {
        Segment::Int(n) => matches!(id, ElementId::Integer(m) if m == n),
        Segment::Str(text) => segment_text(id) == *text || id_text(id, false) == *text,
        _ => false,
    }
}

/// Sorted by top, then left, then path text; `focused` always absent.
pub fn to_nodes(observed: &[Observed]) -> Vec<Node> {
    ordered(observed).into_iter().map(node_of).collect()
}

/// What a target resolved to: an element, or a bare point in logical pixels.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Resolved {
    Element(Node),
    Point(Point<Pixels>),
}

/// A resolved target before conversion, so containment can compare full paths.
enum Found<'a> {
    Element(&'a Observed),
    Point(Point<Pixels>),
}

/// Resolve `target` against the observed elements. An id matches by `id_matches`, an id suffix
/// by the end of the own id, a role target by role and exact name; none is `ElementNotFound`,
/// several `Ambiguous`.
pub fn resolve(observed: &[Observed], target: &Target) -> Result<Resolved, PilotError> {
    Ok(match find(observed, target)? {
        Found::Element(one) => Resolved::Element(node_of(one)),
        Found::Point(at) => Resolved::Point(at),
    })
}

/// The root element `root` resolves to and its descendants by full path, in node order.
pub fn subtree(observed: &[Observed], root: &Target) -> Result<Vec<Node>, PilotError> {
    let Found::Element(top) = find(observed, root)? else {
        return Err(PilotError::new(
            ErrorCode::Unsupported,
            "a subtree root must be an element target, not a point",
        ));
    };
    Ok(ordered(observed)
        .into_iter()
        .filter(|o| o.path.starts_with(&top.path))
        .map(node_of)
        .collect())
}

fn find<'a>(observed: &'a [Observed], target: &Target) -> Result<Found<'a>, PilotError> {
    let ordered = ordered(observed);
    match target {
        Target::Point { x, y } => Ok(Found::Point(point(px(*x), px(*y)))),
        Target::Id { id } => {
            let found: Vec<_> = ordered
                .iter()
                .copied()
                .filter(|o| id_matches(id, &o.path))
                .collect();
            pick(found, None, || {
                let mut message = format!("no element path matches {}", path_text(id));
                let wanted = id.last().map(segment_str).unwrap_or_default();
                let ending = ordered
                    .iter()
                    .filter(|o| own_id_ends_with(o, &wanted))
                    .count();
                if ending > 0 {
                    let noun = if ending == 1 { "element" } else { "elements" };
                    message += &format!("; {{\"id_suffix\": {wanted:?}}} matches {ending} {noun}");
                }
                let mut error = PilotError::new(ErrorCode::ElementNotFound, message);
                error.nearest = nearest(&ordered, id);
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
            let found: Vec<_> = ordered
                .iter()
                .copied()
                .filter(|o| own_id_ends_with(o, id_suffix))
                .collect();
            pick(found, None, || {
                let mut error = PilotError::new(
                    ErrorCode::ElementNotFound,
                    format!("no element's own id ends with {id_suffix:?}"),
                );
                error.nearest = nearest(&ordered, &[Segment::Str(id_suffix.clone())]);
                error
            })
        }
        Target::Role { role, name, nth } => {
            let found: Vec<_> = ordered
                .iter()
                .copied()
                .filter(|o| {
                    o.role
                        .is_some_and(|r| format!("{r:?}").eq_ignore_ascii_case(role))
                        && o.label.as_deref() == Some(name.as_str())
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
            "this target kind is not supported by the headless host",
        )),
    }
}

/// Whether the element's own id, in its short or full text, ends with `suffix`.
fn own_id_ends_with(observed: &Observed, suffix: &str) -> bool {
    observed.path.last().is_some_and(|own| {
        segment_text(own).ends_with(suffix) || id_text(own, false).ends_with(suffix)
    })
}

/// Whether the element's own id, in its short or full text, contains `text`.
fn own_id_contains(observed: &Observed, text: &str) -> bool {
    observed
        .path
        .last()
        .is_some_and(|own| segment_text(own).contains(text) || id_text(own, false).contains(text))
}

/// Roles whose element floats above the window's content and takes the pointer input there.
const OVERLAY_ROLES: [&str; 7] = [
    "Dialog",
    "AlertDialog",
    "Menu",
    "MenuListPopup",
    "ListBox",
    "Tooltip",
    "Alert",
];

/// Whether `role` floats above the window's content.
pub(crate) fn is_overlay_role(role: Role) -> bool {
    OVERLAY_ROLES.contains(&format!("{role:?}").as_str())
}

/// A target ready for pointer input: its node, where the pointer lands, and the element
/// estimated to receive the input there.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Actionable {
    pub node: Node,
    pub at: Point<Pixels>,
    pub hit: Node,
}

/// Resolves `target`; an element must be visible (else `ElementNotFound`), enabled when
/// `need_enabled` (else `Disabled`) and not under an overlay (else `Occluded`, with `hit`).
pub fn actionable(
    observed: &[Observed],
    target: &Target,
    need_enabled: bool,
) -> Result<Actionable, PilotError> {
    let target = match find(observed, target)? {
        Found::Element(one) => one,
        Found::Point(at) => {
            let (x, y) = (f32::from(at.x), f32::from(at.y));
            let found = candidates(observed, x, y);
            let top = smallest(overlays(&found));
            let hit = topmost(
                found
                    .iter()
                    .copied()
                    .filter(|o| top.is_none_or(|t| inside(t, o))),
            )
            .map_or_else(
                || Node::new(vec![], Rect::new(x, y, 0.0, 0.0), true, 0),
                node_of,
            );
            return Ok(Actionable {
                node: hit.clone(),
                at,
                hit,
            });
        }
    };
    if !target.visible {
        return Err(PilotError::new(
            ErrorCode::ElementNotFound,
            "the target matches only a hidden element",
        ));
    }
    if need_enabled && target.disabled == Some(true) {
        return Err(PilotError::new(
            ErrorCode::Disabled,
            "the target element is disabled",
        ));
    }
    let Bounds { origin, size } = target.bounds;
    let cx = f32::from(origin.x) + f32::from(size.width) / 2.0;
    let cy = f32::from(origin.y) + f32::from(size.height) / 2.0;
    let found = candidates(observed, cx, cy);

    // The overlay the target lives in sits on top of everything larger; a smaller overlay
    // outside the target's own subtree is drawn over it and takes the input.
    let owner = smallest(overlays(&found).filter(|o| inside(o, target)));
    let occluder = smallest(overlays(&found).filter(|o| {
        !inside(o, target) && !inside(target, o) && owner.is_none_or(|owner| area(o) < area(owner))
    }));
    if let Some(occluder) = occluder {
        let hit =
            deepest(found.iter().copied().filter(|o| inside(occluder, o))).unwrap_or(occluder);
        let mut error = PilotError::new(
            ErrorCode::Occluded,
            "an overlay covers the target; the input would reach the reported hit",
        );
        error.hit = Some(node_of(hit));
        return Err(error);
    }
    let hit = topmost(found.iter().copied().filter(|o| inside(target, o))).unwrap_or(target);
    Ok(Actionable {
        node: node_of(target),
        at: point(px(cx), px(cy)),
        hit: node_of(hit),
    })
}

/// The visible elements whose bounds contain the point, in node order.
fn candidates(observed: &[Observed], x: f32, y: f32) -> Vec<&Observed> {
    ordered(observed)
        .into_iter()
        .filter(|o| o.visible && o.bounds.contains(&point(px(x), px(y))))
        .collect()
}

fn overlays<'a>(found: &[&'a Observed]) -> impl Iterator<Item = &'a Observed> {
    found
        .iter()
        .copied()
        .filter(|o| o.role.is_some_and(is_overlay_role))
}

/// Whether `observed` is `outer` or one of its descendants, by full path.
fn inside(outer: &Observed, observed: &Observed) -> bool {
    observed.path.starts_with(&outer.path)
}

fn area(observed: &Observed) -> f32 {
    f32::from(observed.bounds.size.width) * f32::from(observed.bounds.size.height)
}

fn smallest<'a>(found: impl Iterator<Item = &'a Observed>) -> Option<&'a Observed> {
    found.min_by(|a, b| area(a).total_cmp(&area(b)))
}

/// The longest full path, the last in node order on a tie.
fn deepest<'a>(found: impl Iterator<Item = &'a Observed>) -> Option<&'a Observed> {
    found.max_by_key(|o| o.path.len())
}

/// The last drawn by paint rank, the longer path on a tie; with no ranks, the deepest.
fn topmost<'a>(found: impl Iterator<Item = &'a Observed>) -> Option<&'a Observed> {
    found.max_by_key(|o| (o.layer.map(|l| l.rank), o.path.len()))
}

/// The one match, or the `nth` when given; `missing` builds the error for an empty choice.
fn pick<'a>(
    found: Vec<&'a Observed>,
    nth: Option<u32>,
    missing: impl FnOnce() -> PilotError,
) -> Result<Found<'a>, PilotError> {
    match (nth, found.as_slice()) {
        (Some(n), _) => match found.get(n as usize) {
            Some(one) => Ok(Found::Element(one)),
            None => {
                let mut error = missing();
                error.message = format!("{} (index {n} of {} matches)", error.message, found.len());
                Err(error)
            }
        },
        (None, []) => Err(missing()),
        (None, [one]) => Ok(Found::Element(one)),
        (None, many) => {
            let mut error = PilotError::new(
                ErrorCode::Ambiguous,
                format!("{} elements match; narrow the target", many.len()),
            );
            error.matches = many.iter().map(|o| node_of(o)).collect();
            Err(error)
        }
    }
}

/// Node order: top, then left, then the path's segment texts.
fn ordered(observed: &[Observed]) -> Vec<&Observed> {
    let mut ordered: Vec<_> = observed.iter().collect();
    ordered.sort_by(|a, b| {
        let (a_at, b_at) = (a.bounds.origin, b.bounds.origin);
        f32::from(a_at.y)
            .total_cmp(&f32::from(b_at.y))
            .then_with(|| f32::from(a_at.x).total_cmp(&f32::from(b_at.x)))
            .then_with(|| path_order(&a.path, &b.path))
    });
    ordered
}

fn path_order(a: &[ElementId], b: &[ElementId]) -> Ordering {
    a.iter()
        .map(ToString::to_string)
        .cmp(b.iter().map(ToString::to_string))
}

/// The last segment names the element itself; earlier segments name its ancestors in order,
/// with any ancestors between them skipped.
fn id_matches(target: &[Segment], path: &[ElementId]) -> bool {
    match (target.split_last(), path.split_last()) {
        (Some((last, before)), Some((own, ancestors))) => {
            segment_matches(last, own) && in_order(before, ancestors) == before.len()
        }
        _ => false,
    }
}

/// How many of the target's segments, walking both from the end, match path ids in order with
/// gaps allowed; greedy, so each segment takes the nearest matching id.
fn in_order(target: &[Segment], path: &[ElementId]) -> usize {
    let mut segments = target.iter().rev().peekable();
    let mut matched = 0;
    for id in path.iter().rev() {
        match segments.peek() {
            Some(segment) if segment_matches(segment, id) => {
                segments.next();
                matched += 1;
            }
            Some(_) => {}
            None => break,
        }
    }
    matched
}

/// Up to five display paths ranked by target segments matched in order, then by whether the own
/// id contains the wanted last segment, then by how long a prefix the own id shares with it; the
/// stable sort keeps node order for ties.
fn nearest(ordered: &[&Observed], target: &[Segment]) -> Vec<Vec<Segment>> {
    let wanted = target.last().map(segment_str).unwrap_or_default();
    let mut ranked: Vec<_> = ordered
        .iter()
        .map(|o| {
            let last = o.path.last().map(segment_text).unwrap_or_default();
            let shared = wanted
                .chars()
                .zip(last.chars())
                .take_while(|(a, b)| a == b)
                .count();
            let contains = own_id_contains(o, &wanted);
            (Reverse((in_order(target, &o.path), contains, shared)), *o)
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked
        .into_iter()
        .take(NEAREST)
        .map(|(_, o)| display_path(&o.path))
        .collect()
}

fn segment_str(segment: &Segment) -> String {
    match segment {
        Segment::Int(n) => n.to_string(),
        Segment::Str(text) => text.clone(),
        _ => String::new(),
    }
}

fn path_text(path: &[Segment]) -> String {
    format!("{:?}", path.iter().map(segment_str).collect::<Vec<_>>())
}

/// The path an agent sees: every code location id dropped except the element's own last one,
/// since source locations are unstable across edits and say nothing about the element.
fn display_path(path: &[ElementId]) -> Vec<Segment> {
    let own = path.len().saturating_sub(1);
    path.iter()
        .enumerate()
        .filter(|(i, id)| *i == own || !matches!(id, ElementId::CodeLocation(_)))
        .map(|(_, id)| segment_of(id))
        .collect()
}

fn node_of(observed: &Observed) -> Node {
    let Bounds { origin, size } = observed.bounds;
    let bounds = Rect::new(
        origin.x.into(),
        origin.y.into(),
        size.width.into(),
        size.height.into(),
    );
    let depth = observed.path.len().saturating_sub(1) as u32;
    let mut node = Node::new(
        display_path(&observed.path),
        bounds,
        observed.visible,
        depth,
    );
    node.role = observed.role.map(|role| format!("{role:?}"));
    node.name = observed.label.clone();
    node.value = observed.value.clone();
    node.checked = observed.checked;
    node.selected = observed.selected;
    node.expanded = observed.expanded;
    node.disabled = observed.disabled;
    node
}

#[cfg(test)]
mod tests {
    /// The exact `ElementSnapshot` Debug forms `debug_says_focused` relies on (gpui-base 0.7.1):
    /// only `focused: Some(true)` counts, so a missed binding or a changed format reads as not
    /// focused and never reaches the asserting getter.
    #[test]
    fn only_the_focused_true_debug_form_reads_as_focused() {
        use super::debug_says_focused;
        let shape = |focused: &str| {
            format!(
                "ElementSnapshot {{ role: Some(TextInput), value: None, visible: true, \
                 focused: {focused}, focus_action: true, disabled: None, label: None }}"
            )
        };
        assert!(debug_says_focused(&shape("Some(true)")));
        assert!(!debug_says_focused(&shape("Some(false)")));
        assert!(!debug_says_focused(&shape("None")));
        assert!(!debug_says_focused(&shape("<binding missed>")));
        assert!(!debug_says_focused("ElementSnapshot { focus: true }"));
    }

    use super::*;
    use gpui_kit::{Role, bounds, point, px, size};
    use gpui_pilot_protocol::{ErrorCode, Segment, Target};

    fn name(text: &str) -> ElementId {
        ElementId::Name(text.to_string().into())
    }

    fn at(path: Vec<ElementId>, x: f32, y: f32) -> Observed {
        Observed::new(path, bounds(point(px(x), px(y)), size(px(40.), px(20.))))
    }

    fn id_target(segments: &[&str]) -> Target {
        Target::Id {
            id: segments
                .iter()
                .map(|s| Segment::Str(s.to_string()))
                .collect(),
        }
    }

    fn element(result: Result<Resolved, PilotError>) -> Node {
        match result {
            Ok(Resolved::Element(node)) => node,
            other => panic!("expected an element: {other:?}"),
        }
    }

    #[test]
    fn segments_keep_integers_and_display_text() {
        assert_eq!(segment_of(&ElementId::Integer(7)), Segment::Int(7));
        assert_eq!(segment_of(&name("a/b")), Segment::Str("a/b".into()));
        assert_eq!(
            segment_of(&ElementId::NamedInteger("row".into(), 3)),
            Segment::Str("row-3".into())
        );
        assert!(segment_matches(&Segment::Int(7), &ElementId::Integer(7)));
        assert!(!segment_matches(&Segment::Int(7), &name("7")));
        assert!(segment_matches(
            &Segment::Str("7".into()),
            &ElementId::Integer(7)
        ));
    }

    #[test]
    fn an_id_matches_its_last_segment_and_earlier_ones_in_order() {
        let seen = vec![at(vec![name("root"), name("panel"), name("count")], 0., 0.)];
        let hits: [&[&str]; 4] = [
            &["count"],
            &["panel", "count"],
            &["root", "count"],
            &["root", "panel", "count"],
        ];
        for ok in hits {
            assert!(resolve(&seen, &id_target(ok)).is_ok(), "{ok:?}");
        }
        let misses: [&[&str]; 4] = [
            &["panel"],
            &["count", "root"],
            &["panel", "root", "count"],
            &["other", "count"],
        ];
        for miss in misses {
            let error = resolve(&seen, &id_target(miss)).unwrap_err();
            assert_eq!(error.code, ErrorCode::ElementNotFound, "{miss:?}");
        }
    }

    #[test]
    fn component_type_segments_can_be_skipped() {
        let button = |group: &str, y| {
            at(
                vec![
                    name(group),
                    name("gpui_component::button::button::Button"),
                    name("gpui_base::button::Button"),
                    ElementId::Integer(1),
                ],
                0.,
                y,
            )
        };
        let seen = vec![button("theme", 0.), button("orientation", 40.)];
        let by_int = Target::Id {
            id: vec![Segment::Str("theme".into()), Segment::Int(1)],
        };
        let node = element(resolve(&seen, &by_int));
        assert_eq!(node.path[0], Segment::Str("theme".into()));
        let by_text = Target::Id {
            id: vec![Segment::Str("theme".into()), Segment::Str("1".into())],
        };
        assert_eq!(element(resolve(&seen, &by_text)).path, node.path);
    }

    #[test]
    fn absolute_source_paths_shorten_to_the_crate_directory() {
        let file = "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/gpui-component-0.7.1/src/dialog/dialog.rs";
        assert_eq!(
            short_location(file, 720, 66),
            "gpui-component-0.7.1/src/dialog/dialog.rs:720:66"
        );
        assert_eq!(short_location("/opt/gen/out.rs", 3, 4), "out.rs:3:4");
        assert_eq!(
            short_location("crates/app/src/ui.rs", 9, 1),
            "crates/app/src/ui.rs:9:1"
        );
    }

    #[test]
    fn a_code_location_id_shows_and_matches_its_short_text() {
        let here = *std::panic::Location::caller();
        let short = short_location(here.file(), here.line(), here.column());
        let location = ElementId::CodeLocation(here);
        assert_eq!(segment_of(&location), Segment::Str(short.clone()));
        assert!(segment_matches(&Segment::Str(short.clone()), &location));
        let child = ElementId::NamedChild(std::sync::Arc::new(location), "content".into());
        let text = format!("{short}-content");
        assert_eq!(segment_of(&child), Segment::Str(text.clone()));
        assert!(segment_matches(&Segment::Str(text), &child));
        assert!(segment_matches(&Segment::Str(child.to_string()), &child));
    }

    fn child(inner: &str, child: &str) -> ElementId {
        ElementId::NamedChild(std::sync::Arc::new(name(inner)), child.to_string().into())
    }

    #[test]
    fn named_children_that_join_alike_get_distinct_ids() {
        let row = child("connection", "toggle-lite");
        let toggle = child("connection-toggle", "lite");
        assert_ne!(segment_of(&row), segment_of(&toggle));
        assert_eq!(
            segment_of(&toggle),
            Segment::Str("connection-toggle-lite".into())
        );
        let seen = vec![at(vec![row.clone()], 0., 0.), at(vec![toggle], 0., 40.)];
        let node = element(resolve(&seen, &id_target(&["connection-toggle-lite"])));
        assert_eq!(node.bounds.y, 40.);
        assert_eq!(
            segment_of(&row),
            Segment::Str("connection[toggle-lite]".into())
        );
        let node = element(resolve(&seen, &id_target(&["connection[toggle-lite]"])));
        assert_eq!(node.bounds.y, 0.);
    }

    #[test]
    fn ids_that_still_join_alike_are_ambiguous_and_name_both() {
        let seen = vec![
            at(
                vec![name("list"), child("connection-toggle", "lite")],
                0.,
                0.,
            ),
            at(
                vec![name("header"), name("connection-toggle-lite")],
                0.,
                40.,
            ),
        ];
        let error = resolve(&seen, &id_target(&["connection-toggle-lite"])).unwrap_err();
        assert_eq!(error.code, ErrorCode::Ambiguous);
        let paths: Vec<_> = error.matches.iter().map(|n| n.path[0].clone()).collect();
        assert_eq!(
            paths,
            vec![Segment::Str("list".into()), Segment::Str("header".into())]
        );
    }

    #[test]
    fn a_slash_is_part_of_one_segment() {
        let seen = vec![
            at(vec![name("a/b")], 0., 0.),
            at(vec![name("a"), name("b")], 0., 40.),
        ];
        let node = element(resolve(&seen, &id_target(&["a/b"])));
        assert_eq!(node.path, vec![Segment::Str("a/b".into())]);
    }

    #[test]
    fn duplicate_matches_are_ambiguous_and_listed() {
        let seen = vec![
            at(vec![name("left"), name("count")], 0., 0.),
            at(vec![name("right"), name("count")], 100., 0.),
        ];
        let error = resolve(&seen, &id_target(&["count"])).unwrap_err();
        assert_eq!(error.code, ErrorCode::Ambiguous);
        assert_eq!(error.matches.len(), 2);
        assert!(resolve(&seen, &id_target(&["right", "count"])).is_ok());
    }

    #[test]
    fn a_miss_names_the_nearest_ids_first() {
        let mut seen: Vec<_> = (0..8)
            .map(|i| at(vec![name(&format!("other{i}"))], 0., 50. * i as f32))
            .collect();
        seen.push(at(vec![name("count")], 0., 500.));
        let error = resolve(&seen, &id_target(&["coun"])).unwrap_err();
        assert_eq!(error.code, ErrorCode::ElementNotFound);
        assert_eq!(error.nearest.len(), 5);
        assert_eq!(error.nearest[0], vec![Segment::Str("count".into())]);
    }

    #[test]
    fn role_and_name_match_with_an_optional_index() {
        let mut first = at(vec![name("a")], 0., 0.);
        first.role = Some(Role::Button);
        first.label = Some("Go".into());
        let mut second = at(vec![name("b")], 0., 40.);
        second.role = Some(Role::Button);
        second.label = Some("Go".into());
        let seen = vec![second, first];
        let target = |nth| Target::Role {
            role: "button".into(),
            name: "Go".into(),
            nth,
        };
        assert_eq!(
            resolve(&seen, &target(None)).unwrap_err().code,
            ErrorCode::Ambiguous
        );
        let node = element(resolve(&seen, &target(Some(1))));
        assert_eq!(node.path, vec![Segment::Str("b".into())]);
        assert_eq!(node.role.as_deref(), Some("Button"));
        assert_eq!(
            resolve(&seen, &target(Some(2))).unwrap_err().code,
            ErrorCode::ElementNotFound
        );
    }

    #[test]
    fn a_point_target_resolves_to_the_point() {
        let target = Target::Point { x: 3.0, y: 4.0 };
        assert_eq!(
            resolve(&[], &target).unwrap(),
            Resolved::Point(point(px(3.), px(4.)))
        );
    }

    #[test]
    fn nodes_sort_by_top_then_left_and_carry_depth() {
        let seen = vec![
            at(vec![name("r"), name("low")], 0., 50.),
            at(vec![name("r"), name("right")], 60., 0.),
            at(vec![name("r"), name("left")], 0., 0.),
        ];
        let nodes = to_nodes(&seen);
        let last: Vec<_> = nodes.iter().map(|n| n.path[1].clone()).collect();
        assert_eq!(
            last,
            ["left", "right", "low"].map(|s| Segment::Str(s.into()))
        );
        assert!(nodes.iter().all(|n| n.depth == 1 && n.focused.is_none()));
        assert_eq!(nodes[0].bounds.w, 40.0);
    }

    fn sized(path: Vec<ElementId>, x: f32, y: f32, w: f32, h: f32) -> Observed {
        Observed::new(path, bounds(point(px(x), px(y)), size(px(w), px(h))))
    }

    fn with_role(mut observed: Observed, role: Role) -> Observed {
        observed.role = Some(role);
        observed
    }

    fn last(node: &Node) -> Option<&Segment> {
        node.path.last()
    }

    #[test]
    fn an_overlay_over_the_target_occludes_it_and_is_the_hit() {
        let seen = vec![
            sized(vec![name("main"), name("open")], 0., 0., 100., 30.),
            with_role(
                sized(vec![name("layer"), name("dialog")], 0., 0., 640., 400.),
                Role::Dialog,
            ),
            sized(
                vec![name("layer"), name("dialog"), name("ok")],
                300.,
                200.,
                60.,
                30.,
            ),
        ];
        let error = actionable(&seen, &id_target(&["open"]), true).unwrap_err();
        assert_eq!(error.code, ErrorCode::Occluded);
        let hit = error.hit.expect("the occluder is reported");
        assert_eq!(last(&hit), Some(&Segment::Str("dialog".into())));
        let ok = actionable(&seen, &id_target(&["ok"]), true).unwrap();
        assert_eq!(last(&ok.hit), Some(&Segment::Str("ok".into())));
        assert_eq!(ok.at, point(px(330.), px(215.)));
    }

    #[test]
    fn a_smaller_overlay_covers_the_larger_ones_content() {
        let seen = vec![
            with_role(
                sized(vec![name("dialog")], 0., 0., 640., 400.),
                Role::Dialog,
            ),
            sized(vec![name("dialog"), name("field")], 100., 100., 200., 30.),
            with_role(sized(vec![name("popup")], 90., 90., 150., 80.), Role::Menu),
            with_role(
                sized(vec![name("popup"), name("item")], 95., 95., 140., 26.),
                Role::MenuItem,
            ),
        ];
        let error = actionable(&seen, &id_target(&["field"]), true).unwrap_err();
        assert_eq!(error.code, ErrorCode::Occluded);
        assert_eq!(
            last(&error.hit.unwrap()),
            Some(&Segment::Str("item".into()))
        );
        let item = actionable(&seen, &id_target(&["item"]), true).unwrap();
        assert_eq!(last(&item.hit), Some(&Segment::Str("item".into())));
    }

    #[test]
    fn disabled_and_hidden_targets_are_refused_with_their_codes() {
        let mut off = sized(vec![name("off")], 0., 0., 40., 20.);
        off.disabled = Some(true);
        let mut gone = sized(vec![name("gone")], 0., 40., 40., 20.);
        gone.visible = false;
        let seen = vec![off, gone];
        let refused = actionable(&seen, &id_target(&["off"]), true).unwrap_err();
        assert_eq!(refused.code, ErrorCode::Disabled);
        assert!(actionable(&seen, &id_target(&["off"]), false).is_ok());
        let hidden = actionable(&seen, &id_target(&["gone"]), false).unwrap_err();
        assert_eq!(hidden.code, ErrorCode::ElementNotFound);
    }

    #[test]
    fn a_point_reports_the_top_hit_without_failing() {
        let seen = vec![
            sized(vec![name("main"), name("open")], 0., 0., 100., 30.),
            with_role(sized(vec![name("toast")], 50., 0., 100., 30.), Role::Alert),
        ];
        let under = actionable(&seen, &Target::Point { x: 60., y: 10. }, true).unwrap();
        assert_eq!(last(&under.hit), Some(&Segment::Str("toast".into())));
        assert_eq!(under.node, under.hit);
        assert_eq!(under.at, point(px(60.), px(10.)));
        let bare = actionable(&seen, &Target::Point { x: 10., y: 10. }, true).unwrap();
        assert_eq!(last(&bare.hit), Some(&Segment::Str("open".into())));
        let empty = actionable(&seen, &Target::Point { x: 500., y: 300. }, true).unwrap();
        assert!(empty.hit.path.is_empty());
    }

    #[test]
    fn a_target_reports_its_deepest_descendant_at_the_centre() {
        let seen = vec![
            sized(vec![name("row")], 0., 0., 200., 40.),
            sized(vec![name("row"), name("label")], 80., 10., 40., 20.),
        ];
        let row = actionable(&seen, &id_target(&["row"]), true).unwrap();
        assert_eq!(last(&row.node), Some(&Segment::Str("row".into())));
        assert_eq!(last(&row.hit), Some(&Segment::Str("label".into())));
    }

    fn location() -> ElementId {
        ElementId::CodeLocation(*std::panic::Location::caller())
    }

    #[test]
    fn an_id_suffix_matches_the_end_of_an_elements_own_id() {
        let seen = vec![
            at(vec![name("entries"), name("0:json.yml")], 0., 0.),
            at(
                vec![
                    name("entries"),
                    name("0:json.yml"),
                    name("method:0:json.yml"),
                ],
                0.,
                40.,
            ),
            at(vec![name("entries"), name("0:slow.yml")], 0., 80.),
        ];
        let suffix = |text: &str| Target::IdSuffix {
            id_suffix: text.into(),
        };
        let slow = element(resolve(&seen, &suffix("slow.yml")));
        assert_eq!(last(&slow), Some(&Segment::Str("0:slow.yml".into())));
        let both = resolve(&seen, &suffix("json.yml")).unwrap_err();
        assert_eq!(both.code, ErrorCode::Ambiguous);
        assert_eq!(both.matches.len(), 2);
        assert_eq!(
            resolve(&seen, &suffix("nope")).unwrap_err().code,
            ErrorCode::ElementNotFound
        );
        assert_eq!(
            resolve(&seen, &suffix("")).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn an_exact_id_miss_suggests_containing_ids_and_the_suffix_form() {
        let seen = vec![
            at(vec![name("aaa")], 0., 0.),
            at(vec![name("json")], 0., 20.),
            at(vec![name("entries"), name("0:json.yml")], 0., 40.),
        ];
        let error = resolve(&seen, &id_target(&["json.yml"])).unwrap_err();
        assert_eq!(error.code, ErrorCode::ElementNotFound);
        assert_eq!(
            error.nearest[0],
            vec![
                Segment::Str("entries".into()),
                Segment::Str("0:json.yml".into())
            ]
        );
        assert!(
            error
                .message
                .contains(r#"{"id_suffix": "json.yml"} matches 1 element"#),
            "{}",
            error.message
        );
    }

    #[test]
    fn node_paths_drop_ancestor_code_locations_but_keep_an_own_one() {
        let loc = location();
        let seen = vec![
            at(vec![name("list"), loc.clone(), name("row")], 0., 0.),
            at(vec![name("list"), loc.clone()], 0., 40.),
        ];
        let nodes = to_nodes(&seen);
        assert_eq!(
            nodes[0].path,
            vec![Segment::Str("list".into()), Segment::Str("row".into())]
        );
        assert_eq!(
            nodes[1].path,
            vec![Segment::Str("list".into()), segment_of(&loc)]
        );
        assert_eq!(nodes[0].depth, 2, "depth counts every ancestor");
        let row = element(resolve(
            &seen,
            &Target::Id {
                id: nodes[0].path.clone(),
            },
        ));
        assert_eq!(row.path, nodes[0].path);
        let old = Target::Id {
            id: vec![segment_of(&loc), Segment::Str("row".into())],
        };
        assert_eq!(
            element(resolve(&seen, &old)).path,
            nodes[0].path,
            "location text still matches"
        );
    }

    #[test]
    fn containment_uses_full_paths_when_an_overlay_id_is_a_code_location() {
        let loc = location();
        let seen = vec![
            sized(vec![name("main"), name("open")], 0., 0., 100., 30.),
            with_role(
                sized(vec![name("layer"), loc.clone()], 0., 0., 640., 400.),
                Role::Dialog,
            ),
            sized(
                vec![name("layer"), loc.clone(), name("ok")],
                300.,
                200.,
                60.,
                30.,
            ),
        ];
        let ok = actionable(&seen, &id_target(&["ok"]), true).unwrap();
        assert_eq!(last(&ok.hit), Some(&Segment::Str("ok".into())));
        let open = actionable(&seen, &id_target(&["open"]), true).unwrap_err();
        assert_eq!(open.code, ErrorCode::Occluded);
        let root = Target::Id {
            id: vec![Segment::Str("layer".into()), segment_of(&loc)],
        };
        assert_eq!(subtree(&seen, &root).unwrap().len(), 2);
    }

    fn ranked(mut observed: Observed, rank: u32) -> Observed {
        observed.layer = Some(crate::Layer {
            rank,
            overlay: None,
        });
        observed
    }

    #[test]
    fn a_point_hits_the_last_drawn_element_not_the_deepest() {
        let deep = vec![name("a"), name("b"), name("c"), name("row")];
        let seen = vec![
            ranked(sized(deep, 0., 0., 200., 40.), 3),
            ranked(sized(vec![name("bar"), name("mark")], 0., 0., 100., 40.), 9),
        ];
        let at = Target::Point { x: 10., y: 10. };
        let hit = actionable(&seen, &at, true).unwrap();
        assert_eq!(last(&hit.hit), Some(&Segment::Str("mark".into())));
        // Without a tree no element has a rank, and the deepest path wins as before.
        let bare: Vec<_> = seen
            .into_iter()
            .map(|mut o| {
                o.layer = None;
                o
            })
            .collect();
        let hit = actionable(&bare, &at, true).unwrap();
        assert_eq!(last(&hit.hit), Some(&Segment::Str("row".into())));
    }

    #[test]
    fn a_targets_hit_is_its_last_drawn_descendant_and_a_tie_goes_deeper() {
        let seen = vec![
            ranked(sized(vec![name("row")], 0., 0., 200., 40.), 1),
            ranked(
                sized(
                    vec![name("row"), name("x"), name("y"), name("deep")],
                    80.,
                    10.,
                    40.,
                    20.,
                ),
                2,
            ),
            ranked(
                sized(vec![name("row"), name("badge")], 90., 15., 20., 10.),
                5,
            ),
        ];
        let row = actionable(&seen, &id_target(&["row"]), true).unwrap();
        assert_eq!(last(&row.hit), Some(&Segment::Str("badge".into())));
        // A roleless child shares its roled ancestor's rank; the longer path wins the tie.
        let tie = vec![
            ranked(sized(vec![name("row")], 0., 0., 200., 40.), 1),
            ranked(
                sized(vec![name("row"), name("label")], 80., 10., 40., 20.),
                1,
            ),
        ];
        let row = actionable(&tie, &id_target(&["row"]), true).unwrap();
        assert_eq!(last(&row.hit), Some(&Segment::Str("label".into())));
    }

    #[test]
    fn an_id_target_under_a_later_drawn_unobserved_overlay_is_not_occluded() {
        // A popover's surface carries no observed role, so id targets keep the role-based
        // estimate: the later-drawn mark outside the target neither occludes it nor is its hit.
        let mut mark = sized(vec![name("pop"), name("mark")], 0., 0., 200., 40.);
        mark.layer = Some(crate::Layer {
            rank: 7,
            overlay: Some(gpui_kit::accesskit::NodeId(42)),
        });
        let seen = vec![
            ranked(
                sized(vec![name("main"), name("under")], 0., 0., 200., 40.),
                2,
            ),
            mark,
        ];
        let under = actionable(&seen, &id_target(&["under"]), true).unwrap();
        assert_eq!(last(&under.node), Some(&Segment::Str("under".into())));
        assert_eq!(last(&under.hit), Some(&Segment::Str("under".into())));
    }
}
