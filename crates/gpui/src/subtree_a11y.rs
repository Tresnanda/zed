// Included in window/a11y.rs. Keep AccessKit identities, focus and actions;
// publish changed node geometry without a prepaint or a replacement tree.
impl A11y {
    pub(crate) fn translate_subtrees(
        &mut self,
        deltas: &std::collections::HashMap<NodeId, Point<Pixels>>,
        scale: f32,
    ) -> Option<TreeUpdate> {
        let update = self.subtree_last_update.as_mut()?;
        let mut deltas = deltas.clone();
        let children: std::collections::HashMap<_, _> = update.nodes.iter().map(|(id, n)| (*id, n.children().to_vec())).collect();
        let mut pending = vec![(ROOT_NODE_ID, Point::default())];
        while let Some((id, inherited)) = pending.pop() {
            let delta = *deltas.entry(id).or_insert(inherited);
            if let Some(children) = children.get(&id) {
                for child in children {
                    pending.push((*child, delta));
                }
            }
        }
        let mut changed = Vec::new();
        for (id, node) in &mut update.nodes {
            let Some(delta) = deltas.get(id) else {
                continue;
            };
            if *delta == Default::default() {
                continue;
            }
            if let Some(bounds) = self.node_bounds.get_mut(id) {
                bounds.origin += *delta;
            }
            if let Some(mut rect) = node.bounds() {
                let x = delta.x.as_f32() as f64 * scale as f64;
                let y = delta.y.as_f32() as f64 * scale as f64;
                rect.x0 += x;
                rect.x1 += x;
                rect.y0 += y;
                rect.y1 += y;
                node.set_bounds(rect);
                changed.push((*id, node.clone()));
            }
        }
        if changed.is_empty() {
            return None;
        }
        self.debug.capture(update, self.nodes.focus, self.nodes.active_descendant, self.window_title.as_ref(), Default::default());
        Some(TreeUpdate { nodes: changed, tree: None, tree_id: update.tree_id, focus: update.focus })
    }
}
