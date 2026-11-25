//! Paint order from GPUI's accessibility tree. GPUI builds the tree in prepaint order, deferred
//! draws (popovers, menus) last, so a pre-order walk ranks every roled element by when it was
//! drawn: a later rank is drawn, and hit, above an earlier one.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use gpui_kit::accesskit::{Node, NodeId, Rect, Role, TreeUpdate};
use gpui_kit::{Bounds, ElementId, Pixels, bounds, point, px, size};
use rustc_hash::FxHasher;

use crate::resolve::{Authored, Observed, is_overlay_role};

/// Where an observed element sits in the paint order.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layer {
    /// Pre-order position in the tree: a higher rank is drawn, and hit, above a lower one.
    pub rank: u32,
    /// The nearest overlay-role node at or above the element's node.
    pub overlay: Option<NodeId>,
}

/// One tree's ranks and overlay membership, by accesskit node id. A test hook, not a contract.
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct Layering {
    rank: HashMap<NodeId, u32>,
    overlay: HashMap<NodeId, NodeId>,
}

impl Layering {
    /// Walks `update` depth first from `update.tree`'s root, children in their listed order.
    pub fn from_update(update: &TreeUpdate) -> Self {
        let nodes: HashMap<NodeId, _> = update.nodes.iter().map(|(id, n)| (*id, n)).collect();
        let root = update.tree.as_ref().map_or(NodeId(0), |tree| tree.root);
        let mut layering = Self::default();
        // An explicit stack, children pushed in reverse, keeps the walk in listed order without
        // recursing on deep trees; each entry carries its nearest overlay ancestor down.
        let mut stack = vec![(root, None)];
        while let Some((id, overlay)) = stack.pop() {
            // A node already ranked (listed under two parents, or reached through a cycle) keeps
            // its first rank and is not walked again, so the walk always ends.
            if layering.rank.contains_key(&id) {
                continue;
            }
            let Some(node) = nodes.get(&id) else { continue };
            let overlay = if is_overlay_role(node.role()) {
                Some(id)
            } else {
                overlay
            };
            layering.rank.insert(id, layering.rank.len() as u32);
            if let Some(overlay) = overlay {
                layering.overlay.insert(id, overlay);
            }
            stack.extend(node.children().iter().rev().map(|&child| (child, overlay)));
        }
        layering
    }

    /// Whether the tree has a node with this id.
    pub fn contains(&self, id: NodeId) -> bool {
        self.rank.contains_key(&id)
    }

    /// The layer of the longest prefix of `path` that has a node; `None` when none has.
    pub fn layer(&self, path: &[ElementId]) -> Option<Layer> {
        (1..=path.len())
            .rev()
            .find_map(|len| self.of(node_id(&path[..len])))
    }

    /// The layer of the node with this id; `None` when the tree has no such node.
    pub fn of(&self, id: NodeId) -> Option<Layer> {
        self.rank.get(&id).map(|&rank| Layer {
            rank,
            overlay: self.overlay.get(&id).copied(),
        })
    }

    /// Sets each element's `layer` from its path.
    pub fn assign(&self, observed: &mut [Observed]) {
        for o in observed {
            o.layer = self.layer(&o.path);
        }
    }
}

/// Kit ListItem rows that GPUI's accessibility tree carries but no observed path reaches,
/// as observed rows under synthetic `a11y-list-<k>` / `a11y-listitem-<n>` segments.
///
/// Rows come in tree pre-order. `scale` converts the tree's device pixels to logical ones;
/// `window` is the logical window, which a visible row's centre must lie inside.
pub fn a11y_only(
    update: &TreeUpdate,
    layering: &Layering,
    observed: &[Observed],
    scale: f32,
    window: Bounds<Pixels>,
) -> Vec<Observed> {
    let known = known_paths(observed);
    let nodes: HashMap<NodeId, &Node> = update.nodes.iter().map(|(id, n)| (*id, n)).collect();
    let is_row = |id: &NodeId| nodes.get(id).is_some_and(|n| n.role() == Role::ListItem);

    // A group (a node with an unknown ListItem child) gets its path prefix when first reached,
    // so `k` follows pre-order; its rows then extend that prefix.
    let mut groups: HashMap<NodeId, Vec<ElementId>> = HashMap::new();
    let mut lists_under: HashMap<Option<NodeId>, usize> = HashMap::new();
    let mut rows = Vec::new();
    walk(update, &nodes, &known, |id, node, parent, anchor| {
        let unknown_row = |c: &NodeId| is_row(c) && !known.contains_key(c);
        if node.children().iter().any(unknown_row) {
            let prefix = match known.get(&id) {
                Some(path) => path.to_vec(),
                None => {
                    let k = lists_under.entry(anchor).or_default();
                    *k += 1;
                    let mut path = anchor
                        .and_then(|a| known.get(&a))
                        .map_or_else(Vec::new, |p| p.to_vec());
                    path.push(ElementId::Name(format!("a11y-list-{k}").into()));
                    path
                }
            };
            groups.insert(id, prefix);
        }
        if node.role() == Role::ListItem
            && !known.contains_key(&id)
            && let Some(group) = parent
            && let (Some(prefix), Some(g)) = (groups.get(&group), nodes.get(&group))
        {
            let n = 1 + g
                .children()
                .iter()
                .filter(|c| is_row(c))
                .position(|c| *c == id)
                .unwrap_or_default();
            let mut path = prefix.clone();
            path.push(ElementId::Name(format!("a11y-listitem-{n}").into()));
            let area = node
                .bounds()
                .map_or_else(Bounds::default, |r| logical(r, scale));
            let centre = area.center();
            let in_group = g
                .bounds()
                .is_none_or(|r| logical(r, scale).contains(&centre));
            let mut row = Observed::new(path, area);
            row.role = Some(Role::ListItem);
            row.selected = node.is_selected();
            row.visible = node.bounds().is_some() && in_group && window.contains(&centre);
            row.layer = layering.of(id);
            rows.push(row);
        }
    });
    rows
}

/// Author-id leaves no observed path reaches, as rows under the nearest known ancestor.
///
/// A row's path is that ancestor's path plus the author id, and rows come in tree pre-order,
/// which is the app's push order. `scale` and `window` are as for [`a11y_only`].
pub fn authored(
    update: &TreeUpdate,
    layering: &Layering,
    observed: &[Observed],
    scale: f32,
    window: Bounds<Pixels>,
) -> Vec<Observed> {
    let known = known_paths(observed);
    let nodes: HashMap<NodeId, &Node> = update.nodes.iter().map(|(id, n)| (*id, n)).collect();
    let mut rows = Vec::new();
    walk(update, &nodes, &known, |id, node, parent, anchor| {
        let (Some(author), Some(parent)) = (node.author_id(), parent) else {
            return;
        };
        if !is_authored(node, id, &known) {
            return;
        }
        let mut path = anchor
            .and_then(|a| known.get(&a))
            .map_or_else(Vec::new, |p| p.to_vec());
        path.push(ElementId::Name(author.to_string().into()));
        let area = node
            .bounds()
            .map_or_else(Bounds::default, |r| logical(r, scale));
        let centre = area.center();
        // The parent's area bounds visibility and, cut to the window, every fallback point.
        let parent_area = nodes
            .get(&parent)
            .and_then(|p| p.bounds())
            .map(|r| logical(r, scale));
        let mut row = Observed::new(path, area);
        row.role = Some(node.role());
        row.label = node.label().map(str::to_owned);
        row.selected = node.is_selected();
        row.visible = node.bounds().is_some()
            && parent_area.is_none_or(|p| p.contains(&centre))
            && window.contains(&centre);
        row.layer = layering.of(id);
        let clip = parent_area.map_or(window, |p| p.intersect(&window));
        row.authored = Some(Authored::new(parent, clip));
        rows.push(row);
    });
    rows
}

/// Every observed path prefix by its node id: a node with one of these ids is already
/// reported, and its prefix is the real path synthetic segments hang from.
fn known_paths(observed: &[Observed]) -> HashMap<NodeId, &[ElementId]> {
    let mut known: HashMap<NodeId, &[ElementId]> = HashMap::new();
    for o in observed {
        for len in 1..=o.path.len() {
            known
                .entry(node_id(&o.path[..len]))
                .or_insert(&o.path[..len]);
        }
    }
    known
}

/// A tree rect in device pixels as logical bounds.
fn logical(r: Rect, scale: f32) -> Bounds<Pixels> {
    let s = f64::from(scale);
    bounds(
        point(px((r.x0 / s) as f32), px((r.y0 / s) as f32)),
        size(px((r.width() / s) as f32), px((r.height() / s) as f32)),
    )
}

/// Whether a node is an authored canvas shape: an author-id leaf no observed path reaches.
fn is_authored(node: &Node, id: NodeId, known: &HashMap<NodeId, &[ElementId]>) -> bool {
    node.author_id().is_some() && node.children().is_empty() && !known.contains_key(&id)
}

/// Visits each node once in pre-order from the root, with its parent and nearest known
/// ancestor. An explicit stack, children pushed in reverse, keeps the listed order.
fn walk<'a>(
    update: &TreeUpdate,
    nodes: &HashMap<NodeId, &'a Node>,
    known: &HashMap<NodeId, &[ElementId]>,
    mut visit: impl FnMut(NodeId, &'a Node, Option<NodeId>, Option<NodeId>),
) {
    let root = update.tree.as_ref().map_or(NodeId(0), |tree| tree.root);
    let mut seen = HashSet::new();
    let mut stack: Vec<(NodeId, Option<NodeId>, Option<NodeId>)> = vec![(root, None, None)];
    while let Some((id, parent, anchor)) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(&node) = nodes.get(&id) else {
            continue;
        };
        visit(id, node, parent, anchor);
        let below = if known.contains_key(&id) {
            Some(id)
        } else {
            anchor
        };
        stack.extend(
            node.children()
                .iter()
                .rev()
                .map(|&child| (child, Some(id), below)),
        );
    }
}

/// GPUI's accesskit node id for the element at `path`: `GlobalElementId::accesskit_node_id` is
/// the path hash, folded one id at a time by an `FxHasher` seeded with the prefix's hash.
#[doc(hidden)]
pub fn node_id(path: &[ElementId]) -> NodeId {
    NodeId(path.iter().fold(0, |prefix, id| {
        let mut hasher = FxHasher::with_seed(prefix as usize);
        id.hash(&mut hasher);
        hasher.finish()
    }))
}

#[cfg(test)]
mod tests {
    use std::hash::{Hash, Hasher};

    use rustc_hash::FxHasher;

    use super::*;
    use crate::Authored;
    use crate::Observed;
    use gpui_kit::accesskit::{Node, Role, Tree, TreeId, TreeUpdate};
    use gpui_kit::{bounds, point, px, size};

    fn path(segments: &[&str]) -> Vec<ElementId> {
        segments
            .iter()
            .map(|s| ElementId::Name(s.to_string().into()))
            .collect()
    }

    /// root -> [main -> [row], pop (Dialog) -> [item]], with node ids hashed from element
    /// paths. Nodes are listed children first, as GPUI pops them, so the ranks can only come
    /// from the walk.
    fn update() -> TreeUpdate {
        let root = NodeId(1);
        let main = node_id(&path(&["main"]));
        let row = node_id(&path(&["main", "row"]));
        let pop = node_id(&path(&["pop"]));
        let item = node_id(&path(&["pop", "item"]));
        let mut root_node = Node::new(Role::Window);
        root_node.set_children(vec![main, pop]);
        let mut main_node = Node::new(Role::Group);
        main_node.set_children(vec![row]);
        let mut pop_node = Node::new(Role::Dialog);
        pop_node.set_children(vec![item]);
        TreeUpdate {
            nodes: vec![
                (item, Node::new(Role::Button)),
                (pop, pop_node),
                (row, Node::new(Role::Button)),
                (main, main_node),
                (root, root_node),
            ],
            tree: Some(Tree::new(root)),
            tree_id: TreeId::ROOT,
            focus: root,
        }
    }

    #[test]
    fn ranks_follow_a_pre_order_walk_from_the_root() {
        let layering = Layering::from_update(&update());
        let rank = |p: &[&str]| layering.layer(&path(p)).expect("ranked").rank;
        assert_eq!(
            [
                rank(&["main"]),
                rank(&["main", "row"]),
                rank(&["pop"]),
                rank(&["pop", "item"])
            ],
            [1, 2, 3, 4]
        );
        assert!(layering.contains(NodeId(1)));
    }

    #[test]
    fn an_element_inside_a_dialog_belongs_to_that_overlay() {
        let layering = Layering::from_update(&update());
        let pop = node_id(&path(&["pop"]));
        assert_eq!(
            layering.layer(&path(&["pop", "item"])).unwrap().overlay,
            Some(pop)
        );
        assert_eq!(layering.layer(&path(&["pop"])).unwrap().overlay, Some(pop));
        assert_eq!(
            layering.layer(&path(&["main", "row"])).unwrap().overlay,
            None
        );
    }

    #[test]
    fn a_roleless_element_takes_its_nearest_ranked_ancestor() {
        let layering = Layering::from_update(&update());
        assert_eq!(
            layering.layer(&path(&["main", "row", "label"])),
            layering.layer(&path(&["main", "row"]))
        );
        assert_eq!(layering.layer(&path(&["elsewhere", "x"])), None);
    }

    #[test]
    fn a_node_listed_under_two_parents_is_ranked_once() {
        let root = NodeId(1);
        let [a, b, shared] = [path(&["a"]), path(&["b"]), path(&["shared"])].map(|p| node_id(&p));
        let mut root_node = Node::new(Role::Window);
        root_node.set_children(vec![a, b]);
        let mut a_node = Node::new(Role::Group);
        a_node.set_children(vec![shared]);
        let mut b_node = Node::new(Role::Group);
        b_node.set_children(vec![shared]);
        let layering = Layering::from_update(&TreeUpdate {
            nodes: vec![
                (root, root_node),
                (a, a_node),
                (b, b_node),
                (shared, Node::new(Role::Button)),
            ],
            tree: Some(Tree::new(root)),
            tree_id: TreeId::ROOT,
            focus: root,
        });
        let rank = |p: &[&str]| layering.layer(&path(p)).expect("ranked").rank;
        assert_eq!([rank(&["a"]), rank(&["shared"]), rank(&["b"])], [1, 2, 3]);
    }

    #[test]
    fn a_node_id_folds_the_path_as_gpuis_global_id_does() {
        // gpui-pre 0.3.11 `extend_path_hash`: each id hashed by an FxHasher seeded with the
        // prefix's hash, starting from 0.
        let p = path(&["a", "b"]);
        let expected = p.iter().fold(0_u64, |prefix, id| {
            let mut hasher = FxHasher::with_seed(prefix as usize);
            id.hash(&mut hasher);
            hasher.finish()
        });
        assert_eq!(node_id(&p), NodeId(expected));
        assert_ne!(node_id(&p), node_id(&path(&["b", "a"])));
    }

    #[test]
    fn assign_sets_each_elements_layer() {
        let layering = Layering::from_update(&update());
        let area = bounds(point(px(0.), px(0.)), size(px(10.), px(10.)));
        let mut seen = vec![
            Observed::new(path(&["pop", "item"]), area),
            Observed::new(path(&["nowhere"]), area),
        ];
        layering.assign(&mut seen);
        assert_eq!(seen[0].layer.map(|l| l.rank), Some(4));
        assert_eq!(seen[1].layer, None);
    }

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Rect {
        Rect { x0, y0, x1, y1 }
    }

    fn item(r: Rect) -> Node {
        let mut n = Node::new(Role::ListItem);
        n.set_bounds(r);
        n
    }

    fn group(role: Role, r: Rect, children: Vec<NodeId>) -> Node {
        let mut n = Node::new(role);
        n.set_bounds(r);
        n.set_children(children);
        n
    }

    fn tree(root_children: Vec<NodeId>, mut nodes: Vec<(NodeId, Node)>) -> TreeUpdate {
        let root = NodeId(1);
        let mut window = Node::new(Role::Window);
        window.set_children(root_children);
        nodes.push((root, window));
        TreeUpdate {
            nodes,
            tree: Some(Tree::new(root)),
            tree_id: TreeId::ROOT,
            focus: root,
        }
    }

    fn screen() -> Bounds<Pixels> {
        bounds(point(px(0.), px(0.)), size(px(1000.), px(1000.)))
    }

    fn rows(update: &TreeUpdate, observed: &[Observed], scale: f32) -> Vec<Observed> {
        let layering = Layering::from_update(update);
        a11y_only(update, &layering, observed, scale, screen())
    }

    fn names(rows: &[Observed]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|o| o.path.iter().map(|s| format!("{s}")).collect())
            .collect()
    }

    #[test]
    fn an_unknown_list_at_the_root_numbers_its_rows() {
        let list = NodeId(100);
        let mut r2 = item(rect(0., 20., 200., 40.));
        r2.set_selected(true);
        let update = tree(
            vec![list],
            vec![
                (NodeId(101), item(rect(0., 0., 200., 20.))),
                (NodeId(102), r2),
                (NodeId(103), Node::new(Role::Button)),
                (
                    list,
                    group(
                        Role::List,
                        rect(0., 0., 200., 100.),
                        vec![NodeId(101), NodeId(102), NodeId(103)],
                    ),
                ),
            ],
        );
        let found = rows(&update, &[], 1.0);
        assert_eq!(
            names(&found),
            [
                vec!["a11y-list-1".to_string(), "a11y-listitem-1".into()],
                vec!["a11y-list-1".to_string(), "a11y-listitem-2".into()],
            ]
        );
        assert_eq!(found[0].role, Some(Role::ListItem));
        assert_eq!(found[1].selected, Some(true));
        assert_eq!(found[0].label, None);
        assert_eq!(
            found[1].bounds,
            bounds(point(px(0.), px(20.)), size(px(200.), px(20.)))
        );
        assert!(found.iter().all(|o| o.visible));
    }

    #[test]
    fn a_row_with_a_registered_element_inside_is_skipped_but_counted() {
        let list = node_id(&path(&["list"]));
        let known = node_id(&path(&["list", "row"]));
        let update = tree(
            vec![list],
            vec![
                (known, item(rect(0., 0., 200., 20.))),
                (NodeId(102), item(rect(0., 20., 200., 40.))),
                (
                    list,
                    group(
                        Role::List,
                        rect(0., 0., 200., 100.),
                        vec![known, NodeId(102)],
                    ),
                ),
            ],
        );
        let registered = Observed::new(
            path(&["list", "row", "opt"]),
            bounds(point(px(0.), px(0.)), size(px(200.), px(20.))),
        );
        let found = rows(&update, &[registered], 1.0);
        assert_eq!(
            names(&found),
            [vec!["list".to_string(), "a11y-listitem-2".into()]]
        );
    }

    #[test]
    fn two_unknown_lists_under_a_known_ancestor_get_their_own_index() {
        let main = node_id(&path(&["main"]));
        let (l1, l2) = (NodeId(100), NodeId(200));
        let update = tree(
            vec![main],
            vec![
                (NodeId(101), item(rect(0., 0., 100., 20.))),
                (NodeId(201), item(rect(100., 0., 200., 20.))),
                (
                    l1,
                    group(Role::List, rect(0., 0., 100., 100.), vec![NodeId(101)]),
                ),
                (
                    l2,
                    group(Role::List, rect(100., 0., 200., 100.), vec![NodeId(201)]),
                ),
                (
                    main,
                    group(Role::Group, rect(0., 0., 400., 400.), vec![l1, l2]),
                ),
            ],
        );
        let known = Observed::new(
            path(&["main"]),
            bounds(point(px(0.), px(0.)), size(px(400.), px(400.))),
        );
        assert_eq!(
            names(&rows(&update, &[known], 1.0)),
            [
                vec![
                    "main".to_string(),
                    "a11y-list-1".into(),
                    "a11y-listitem-1".into()
                ],
                vec![
                    "main".to_string(),
                    "a11y-list-2".into(),
                    "a11y-listitem-1".into()
                ],
            ]
        );
    }

    #[test]
    fn sections_restarting_their_position_still_get_distinct_numbers() {
        let list = NodeId(100);
        let mut nodes = Vec::new();
        let mut ids = Vec::new();
        for (i, pos) in [1usize, 2, 1, 2].into_iter().enumerate() {
            let mut row = item(rect(0., 20. * i as f64, 200., 20. * (i + 1) as f64));
            row.set_position_in_set(pos);
            let id = NodeId(101 + i as u64);
            ids.push(id);
            nodes.push((id, row));
        }
        nodes.push((list, group(Role::List, rect(0., 0., 200., 100.), ids)));
        let last: Vec<String> = rows(&tree(vec![list], nodes), &[], 1.0)
            .iter()
            .map(|o| format!("{}", o.path.last().unwrap()))
            .collect();
        assert_eq!(
            last,
            [
                "a11y-listitem-1",
                "a11y-listitem-2",
                "a11y-listitem-3",
                "a11y-listitem-4"
            ]
        );
    }

    #[test]
    fn scale_divides_bounds() {
        let list = NodeId(100);
        let update = tree(
            vec![list],
            vec![
                (NodeId(101), item(rect(0., 40., 400., 80.))),
                (
                    list,
                    group(Role::List, rect(0., 0., 400., 200.), vec![NodeId(101)]),
                ),
            ],
        );
        assert_eq!(
            rows(&update, &[], 2.0)[0].bounds,
            bounds(point(px(0.), px(20.)), size(px(200.), px(20.)))
        );
    }

    #[test]
    fn a_row_without_bounds_or_off_the_window_is_not_visible() {
        let list = NodeId(100);
        let mut unbounded_list = Node::new(Role::List);
        unbounded_list.set_children(vec![NodeId(101), NodeId(102), NodeId(103)]);
        let update = tree(
            vec![list],
            vec![
                (NodeId(101), Node::new(Role::ListItem)),
                (NodeId(102), item(rect(0., 1990., 200., 2010.))),
                (NodeId(103), item(rect(0., 0., 200., 20.))),
                (list, unbounded_list),
            ],
        );
        // The list has no bounds, so only the window clips: 101 has no bounds of its own and
        // 102's centre (y 2000) is below the 1000 px screen.
        let found = rows(&update, &[], 1.0);
        assert_eq!(
            found.iter().map(|o| o.visible).collect::<Vec<_>>(),
            [false, false, true]
        );
    }

    #[test]
    fn a_row_whose_centre_is_outside_its_list_is_not_visible() {
        let list = NodeId(100);
        let update = tree(
            vec![list],
            vec![
                (NodeId(101), item(rect(0., 0., 200., 20.))),
                (NodeId(102), item(rect(0., 100., 200., 120.))),
                (
                    list,
                    group(
                        Role::List,
                        rect(0., 0., 200., 100.),
                        vec![NodeId(101), NodeId(102)],
                    ),
                ),
            ],
        );
        let found = rows(&update, &[], 1.0);
        assert_eq!(
            found.iter().map(|o| o.visible).collect::<Vec<_>>(),
            [true, false]
        );
    }

    #[test]
    fn a_row_takes_its_own_rank_and_overlay() {
        let (pop, list, row) = (NodeId(50), NodeId(100), NodeId(101));
        let update = tree(
            vec![pop],
            vec![
                (row, item(rect(0., 0., 200., 20.))),
                (list, group(Role::List, rect(0., 0., 200., 100.), vec![row])),
                (
                    pop,
                    group(Role::Dialog, rect(0., 0., 300., 300.), vec![list]),
                ),
            ],
        );
        let layering = Layering::from_update(&update);
        let found = a11y_only(&update, &layering, &[], 1.0, screen());
        assert_eq!(found[0].layer, layering.of(row));
        assert_eq!(found[0].layer.unwrap().overlay, Some(pop));
    }

    #[test]
    fn visibility_compares_logical_bounds_at_scale_two() {
        // Device px: the list spans y 0..100 (logical 0..50). Row centres sit at logical 35
        // and 65. A scaled row against an unscaled list would read [true, true].
        let list = NodeId(100);
        let update = tree(
            vec![list],
            vec![
                (NodeId(101), item(rect(0., 60., 200., 80.))),
                (NodeId(102), item(rect(0., 120., 200., 140.))),
                (
                    list,
                    group(
                        Role::List,
                        rect(0., 0., 200., 100.),
                        vec![NodeId(101), NodeId(102)],
                    ),
                ),
            ],
        );
        let found = rows(&update, &[], 2.0);
        assert_eq!(
            found.iter().map(|o| o.visible).collect::<Vec<_>>(),
            [true, false]
        );
    }

    fn shape(author: &str, r: Rect) -> Node {
        let mut n = Node::new(Role::Group);
        n.set_author_id(author);
        n.set_label(format!("{author} label"));
        n.set_bounds(r);
        n
    }

    fn shapes(update: &TreeUpdate, observed: &[Observed], scale: f32) -> Vec<Observed> {
        authored(
            update,
            &Layering::from_update(update),
            observed,
            scale,
            screen(),
        )
    }

    fn logical(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        bounds(point(px(x), px(y)), size(px(w), px(h)))
    }

    #[test]
    fn an_author_id_leaf_under_a_known_board_becomes_a_row() {
        let board = node_id(&path(&["board"]));
        let mut second = shape("shape-2", rect(400., 140., 500., 240.));
        second.set_selected(true);
        let update = tree(
            vec![board],
            vec![
                (NodeId(500), shape("shape-1", rect(120., 140., 320., 340.))),
                (NodeId(501), second),
                (
                    board,
                    group(
                        Role::Canvas,
                        rect(100., 100., 700., 500.),
                        vec![NodeId(500), NodeId(501)],
                    ),
                ),
            ],
        );
        let known = Observed::new(path(&["board"]), logical(50., 50., 300., 200.));
        let found = shapes(&update, &[known], 2.0);
        assert_eq!(
            names(&found),
            [
                vec!["board".to_string(), "shape-1".into()],
                vec!["board".to_string(), "shape-2".into()]
            ]
        );
        assert_eq!(found[0].role, Some(Role::Group));
        assert_eq!(found[0].label.as_deref(), Some("shape-1 label"));
        assert_eq!((found[0].selected, found[1].selected), (None, Some(true)));
        assert_eq!(found[0].bounds, logical(60., 70., 100., 100.));
        assert!(found[0].visible);
        assert_eq!(
            found[0].authored,
            Some(Authored::new(board, logical(50., 50., 300., 200.)))
        );
        assert_eq!((found[0].value.clone(), found[0].checked), (None, None));
    }

    #[test]
    fn known_nodes_and_containers_with_an_author_id_are_not_rows() {
        let board = node_id(&path(&["board"]));
        let registered = node_id(&path(&["board", "btn"]));
        let mut container = group(Role::Group, rect(0., 0., 50., 50.), vec![NodeId(601)]);
        container.set_author_id("box");
        let update = tree(
            vec![board],
            vec![
                (registered, shape("btn", rect(0., 0., 20., 20.))),
                (NodeId(601), Node::new(Role::Button)),
                (NodeId(600), container),
                (
                    board,
                    group(
                        Role::Canvas,
                        rect(0., 0., 200., 200.),
                        vec![registered, NodeId(600)],
                    ),
                ),
            ],
        );
        let seen = [
            Observed::new(path(&["board"]), logical(0., 0., 200., 200.)),
            Observed::new(path(&["board", "btn"]), logical(0., 0., 20., 20.)),
        ];
        assert!(shapes(&update, &seen, 1.0).is_empty());
    }

    #[test]
    fn a_row_hangs_from_the_nearest_known_ancestor_or_the_root() {
        let main = node_id(&path(&["main"]));
        let update = tree(
            vec![main, NodeId(200)],
            vec![
                (NodeId(101), shape("a", rect(0., 0., 10., 10.))),
                (
                    NodeId(100),
                    group(Role::Canvas, rect(0., 0., 100., 100.), vec![NodeId(101)]),
                ),
                (
                    main,
                    group(Role::Group, rect(0., 0., 400., 400.), vec![NodeId(100)]),
                ),
                (NodeId(201), shape("b", rect(500., 0., 510., 10.))),
                (
                    NodeId(200),
                    group(Role::Canvas, rect(500., 0., 600., 100.), vec![NodeId(201)]),
                ),
            ],
        );
        let known = Observed::new(path(&["main"]), logical(0., 0., 400., 400.));
        let found = shapes(&update, &[known], 1.0);
        assert_eq!(
            names(&found),
            [vec!["main".to_string(), "a".into()], vec!["b".to_string()]]
        );
        // Occlusion keys on the node the shape was pushed under, not on the known ancestor.
        assert_eq!(found[0].authored.map(|a| a.parent), Some(NodeId(100)));
        assert_eq!(found[1].authored.map(|a| a.parent), Some(NodeId(200)));
    }

    #[test]
    fn visibility_needs_bounds_and_a_centre_in_the_parent_and_window_at_scale_two() {
        // Device px; the board spans logical 0..200 x 0..100. `scaled` sits at logical (160, 70),
        // inside; compared unscaled its centre (320, 140) would fall outside.
        let board = node_id(&path(&["board"]));
        let other = node_id(&path(&["other"]));
        let mut bare = Node::new(Role::Group);
        bare.set_author_id("bare");
        let mut unbounded = Node::new(Role::Canvas);
        unbounded.set_children(vec![NodeId(9)]);
        let update = tree(
            vec![board, other],
            vec![
                (NodeId(11), shape("in", rect(20., 20., 60., 60.))),
                (NodeId(12), shape("scaled", rect(300., 120., 340., 160.))),
                (NodeId(13), shape("out", rect(380., 180., 500., 260.))),
                (NodeId(14), bare),
                (
                    board,
                    group(
                        Role::Canvas,
                        rect(0., 0., 400., 200.),
                        vec![NodeId(11), NodeId(12), NodeId(13), NodeId(14)],
                    ),
                ),
                (NodeId(9), shape("off", rect(2100., 0., 2140., 40.))),
                (other, unbounded),
            ],
        );
        let seen = [
            Observed::new(path(&["board"]), logical(0., 0., 200., 100.)),
            Observed::new(path(&["other"]), logical(0., 0., 0., 0.)),
        ];
        let found = shapes(&update, &seen, 2.0);
        assert_eq!(
            found.iter().map(|o| o.visible).collect::<Vec<_>>(),
            [true, true, false, false, false]
        );
        // A parent without bounds clips to the window alone.
        assert_eq!(found[4].authored.map(|a| a.clip), Some(screen()));
    }

    #[test]
    fn the_clip_is_the_parent_cut_to_the_window() {
        let board = node_id(&path(&["board"]));
        let update = tree(
            vec![board],
            vec![
                (NodeId(5), shape("edge", rect(910., 10., 950., 50.))),
                (
                    board,
                    group(Role::Canvas, rect(900., 0., 1200., 400.), vec![NodeId(5)]),
                ),
            ],
        );
        let known = Observed::new(path(&["board"]), logical(900., 0., 300., 400.));
        let found = shapes(&update, &[known], 1.0);
        assert_eq!(
            found[0].authored.map(|a| a.clip),
            Some(logical(900., 0., 100., 400.))
        );
    }

    #[test]
    fn rows_take_their_own_rank_in_push_order_and_their_overlay() {
        let (pop, board) = (NodeId(50), node_id(&path(&["board"])));
        let update = tree(
            vec![pop],
            vec![
                (NodeId(501), shape("low", rect(0., 0., 10., 10.))),
                (NodeId(502), shape("high", rect(0., 0., 10., 10.))),
                (
                    board,
                    group(
                        Role::Canvas,
                        rect(0., 0., 100., 100.),
                        vec![NodeId(501), NodeId(502)],
                    ),
                ),
                (
                    pop,
                    group(Role::Dialog, rect(0., 0., 300., 300.), vec![board]),
                ),
            ],
        );
        let layering = Layering::from_update(&update);
        let known = Observed::new(path(&["board"]), logical(0., 0., 100., 100.));
        let found = authored(&update, &layering, &[known], 1.0, screen());
        assert_eq!(found[0].layer, layering.of(NodeId(501)));
        assert!(found[1].layer.unwrap().rank > found[0].layer.unwrap().rank);
        assert_eq!(found[1].layer.unwrap().overlay, Some(pop));
    }
}
