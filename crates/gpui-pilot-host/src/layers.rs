//! Paint order from GPUI's accessibility tree. GPUI builds the tree in prepaint order, deferred
//! draws (popovers, menus) last, so a pre-order walk ranks every roled element by when it was
//! drawn: a later rank is drawn, and hit, above an earlier one.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use gpui_kit::ElementId;
use gpui_kit::accesskit::{NodeId, TreeUpdate};
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
        (1..=path.len()).rev().find_map(|len| {
            let id = node_id(&path[..len]);
            self.rank.get(&id).map(|&rank| Layer {
                rank,
                overlay: self.overlay.get(&id).copied(),
            })
        })
    }

    /// Sets each element's `layer` from its path.
    pub fn assign(&self, observed: &mut [Observed]) {
        for o in observed {
            o.layer = self.layer(&o.path);
        }
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
}
