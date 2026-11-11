//! Paint order from GPUI's accessibility tree. GPUI builds the tree in prepaint order, deferred
//! draws (popovers, menus) last, so a pre-order walk ranks every roled element by when it was
//! drawn: a later rank is drawn, and hit, above an earlier one.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use gpui_kit::accesskit::{Node, NodeId, Rect, Role, TreeUpdate};
use gpui_kit::{Bounds, ElementId, Pixels, bounds, point, px, size};
use rustc_hash::FxHasher;

use crate::resolve::{Observed, is_overlay_role};

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
    // Every observed path prefix names a node GPUI may also carry; a node with one of these ids
    // is already reported, and its prefix is the real path synthetic segments hang from.
    let mut known: HashMap<NodeId, &[ElementId]> = HashMap::new();
    for o in observed {
        for len in 1..=o.path.len() {
            known
                .entry(node_id(&o.path[..len]))
                .or_insert(&o.path[..len]);
        }
    }
    let nodes: HashMap<NodeId, &Node> = update.nodes.iter().map(|(id, n)| (*id, n)).collect();
    let is_row = |id: &NodeId| nodes.get(id).is_some_and(|n| n.role() == Role::ListItem);
    let logical = |r: Rect| {
        let s = f64::from(scale);
        bounds(
            point(px((r.x0 / s) as f32), px((r.y0 / s) as f32)),
            size(px((r.width() / s) as f32), px((r.height() / s) as f32)),
        )
    };

    // A pre-order walk carrying each node's parent and nearest known ancestor. A group (a node
    // with an unknown ListItem child) gets its path prefix when first reached, so `k` follows
    // pre-order; its rows then extend that prefix.
    let root = update.tree.as_ref().map_or(NodeId(0), |tree| tree.root);
    let mut seen = HashSet::new();
    let mut groups: HashMap<NodeId, Vec<ElementId>> = HashMap::new();
    let mut lists_under: HashMap<Option<NodeId>, usize> = HashMap::new();
    let mut rows = Vec::new();
    let mut stack: Vec<(NodeId, Option<NodeId>, Option<NodeId>)> = vec![(root, None, None)];
    while let Some((id, parent, anchor)) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(node) = nodes.get(&id) else { continue };
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
            let area = node.bounds().map_or_else(Bounds::default, logical);
            let centre = area.center();
            let in_group = g.bounds().is_none_or(|r| logical(r).contains(&centre));
            let mut row = Observed::new(path, area);
            row.role = Some(Role::ListItem);
            row.selected = node.is_selected();
            row.visible = node.bounds().is_some() && in_group && window.contains(&centre);
            row.layer = layering.of(id);
            rows.push(row);
        }
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
    rows
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

    use gpui_kit::accesskit::Rect;
    use gpui_kit::{Bounds, Pixels};

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
}
