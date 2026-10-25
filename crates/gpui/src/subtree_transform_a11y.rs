// Included in window/a11y.rs (subtree_transform_gpui.py). Publish the visual bounds of nodes
// inside transformed scopes without a prepaint or a replacement tree.
impl A11y {
    pub(crate) fn transform_subtrees(
        &mut self,
        rects: &std::collections::HashMap<NodeId, Bounds<Pixels>>,
        scale: f32,
    ) -> Option<TreeUpdate> {
        if rects.is_empty() {
            return None;
        }
        let update = self.subtree_last_update.as_mut()?;
        let mut changed = Vec::new();
        for (id, node) in &mut update.nodes {
            let Some(bounds) = rects.get(id) else { continue };
            self.node_bounds.insert(*id, *bounds);
            node.set_bounds(accesskit::Rect {
                x0: (bounds.origin.x.as_f32() * scale) as f64,
                y0: (bounds.origin.y.as_f32() * scale) as f64,
                x1: ((bounds.origin.x.as_f32() + bounds.size.width.as_f32()) * scale) as f64,
                y1: ((bounds.origin.y.as_f32() + bounds.size.height.as_f32()) * scale) as f64,
            });
            changed.push((*id, node.clone()));
        }
        if changed.is_empty() {
            return None;
        }
        self.debug.capture(update, self.nodes.focus, self.nodes.active_descendant, self.window_title.as_ref(), Default::default());
        Some(TreeUpdate { nodes: changed, tree: None, tree_id: update.tree_id, focus: update.focus })
    }
}
