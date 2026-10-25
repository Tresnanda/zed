// Included in gpui::window. Translation retains raw masks before intersection:
// viewport/ancestor masks stay put; masks established inside the scope move.
#[derive(Clone, Debug, Default)]
#[allow(missing_docs)]
pub struct SubtreeGeometry {
    tags: Vec<(u64, Point<Pixels>)>,
    masks: Vec<SubtreeMask>,
}
#[derive(Clone, Debug)]
struct SubtreeMask {
    bounds: Bounds<Pixels>,
    rounded: Option<crate::RoundedClip>,
    tags: Vec<(u64, Point<Pixels>)>,
}
fn translation_delta(tags: &mut [(u64, Point<Pixels>)], values: &std::collections::HashMap<u64, Point<Pixels>>) -> Point<Pixels> {
    let mut delta = Point::default();
    for (id, old) in tags {
        if let Some(next) = values.get(id) {
            delta += *next - *old;
            *old = *next;
        }
    }
    delta
}
impl SubtreeGeometry {
    #[allow(missing_docs)]
    pub fn translation_changed(&self, values: &std::collections::HashMap<u64, Point<Pixels>>) -> bool {
        self.tags.iter().any(|(id, old)| values.get(id).is_some_and(|next| next != old))
    }
    #[allow(missing_docs)]
    pub fn translate(&mut self, values: &std::collections::HashMap<u64, Point<Pixels>>) -> Point<Pixels> {
        let delta = translation_delta(&mut self.tags, values);
        for mask in &mut self.masks {
            let d = translation_delta(&mut mask.tags, values);
            mask.bounds.origin += d;
            if let Some(shape) = &mut mask.rounded {
                shape.bounds[0] += d.x.as_f32();
                shape.bounds[1] += d.y.as_f32();
            }
        }
        delta
    }
    #[allow(missing_docs)]
    pub fn visual_clip(&self, viewport: Size<Pixels>) -> (Bounds<Pixels>, Vec<[f32; 8]>) {
        let mut clip = Bounds::new(Point::default(), viewport);
        let mut shapes = Vec::new();
        for raw in &self.masks {
            clip = clip.intersect(&raw.bounds);
            // Edge fades (kind 1) do not clip.
            if let Some(s) = raw.rounded.filter(|s| s.kind == ROUNDED_CLIP) {
                shapes.push([s.bounds[0], s.bounds[1], s.bounds[2], s.bounds[3], s.radii[0], s.radii[1], s.radii[2], s.radii[3]]);
            }
        }
        (clip, shapes)
    }
    fn mask(&self, pool: &mut RoundedClipPool, scene: &mut Scene, scale: f32, viewport: Size<Pixels>) -> ContentMask<Pixels> {
        let mut mask = ContentMask { bounds: Bounds::new(Point::default(), viewport), ..Default::default() };
        for raw in &self.masks {
            if let Some(shape) = raw.rounded {
                let b = shape.bounds;
                let (id, generation) =
                    pool.intern_node(Bounds::new(point(px(b[0]), px(b[1])), size(px(b[2]), px(b[3]))), shape.radii, mask, shape.kind);
                pool.sync_node(scene, id, scale);
                mask.rounded_clip = id;
                mask.clip_generation = generation;
            }
            mask.bounds = mask.bounds.intersect(&raw.bounds);
        }
        mask
    }
}
impl Window {
    /// Scope a retained translation. The offset is logical pixels and does not
    /// participate in layout. Cached scene replay preserves these scope tags.
    pub fn with_subtree_translation<R>(&mut self, id: u64, offset: Point<Pixels>, f: impl FnOnce(&mut Self) -> R) -> R {
        self.subtree_tags.push((id, offset));
        self.next_frame.scene.active_subtree_geometry = self.subtree_geometry().map(std::sync::Arc::new);
        let result = self.with_element_offset(offset, f);
        self.subtree_tags.pop();
        self.next_frame.scene.active_subtree_geometry = self.subtree_geometry().map(std::sync::Arc::new);
        result
    }
    #[allow(missing_docs)]
    pub fn subtree_geometry(&self) -> Option<SubtreeGeometry> {
        (!self.subtree_tags.is_empty()).then(|| SubtreeGeometry { tags: self.subtree_tags.clone(), masks: self.subtree_masks.clone() })
    }
    fn push_subtree_mask(&mut self, mask: ContentMask<Pixels>) {
        let inherited = self.content_mask().rounded_clip;
        let rounded =
            (mask.rounded_clip != 0 && mask.rounded_clip != inherited).then(|| self.rounded_clip_pool.nodes[mask.rounded_clip as usize]);
        self.subtree_masks.push(SubtreeMask { bounds: mask.bounds, rounded, tags: self.subtree_tags.clone() });
        self.next_frame.scene.active_subtree_geometry = self.subtree_geometry().map(std::sync::Arc::new);
    }
    fn pop_subtree_mask(&mut self) {
        self.subtree_masks.pop();
        self.next_frame.scene.active_subtree_geometry = self.subtree_geometry().map(std::sync::Arc::new);
    }
    /// Move existing scene operations and native hitboxes without rendering,
    /// layout, prepaint, listener replacement or ancestor invalidation.
    pub fn translate_subtrees(&mut self, updates: &[(u64, Point<Pixels>)]) -> bool {
        if !self.invalidator.not_drawing() {
            return false;
        }
        let values: std::collections::HashMap<_, _> = updates.iter().copied().collect();
        let found: std::collections::HashSet<_> =
            self.rendered_frame.scene.subtree_geometries.values().flat_map(|g| g.tags.iter().map(|(id, _)| *id)).collect();
        let values = values.into_iter().filter(|(id, _)| found.contains(id)).collect::<std::collections::HashMap<_, _>>();
        if values.is_empty() {
            return true;
        }
        if !self
            .rendered_frame
            .scene
            .subtree_geometries
            .values()
            .any(|g| g.tags.iter().any(|(id, old)| values.get(id).is_some_and(|next| next != old)))
        {
            return true;
        }
        let scale = self.scale_factor();
        let viewport = self.viewport_size();
        let mut previous = std::mem::take(&mut self.rendered_frame.scene);
        let mut scene = Scene::default();
        self.rounded_clip_pool.seed(&mut scene, scale);
        // Share immutable provenance across primitives and translate it once
        // per scope/mask context, rather than allocating vectors per glyph.
        let mut translated = std::collections::HashMap::new();
        for (&index, shared) in &mut previous.subtree_geometries {
            let key = std::sync::Arc::as_ptr(shared) as usize;
            let update = translated.entry(key).or_insert_with(|| {
                if !shared.translation_changed(&values) {
                    return None;
                }
                let mut geometry = (**shared).clone();
                let delta = geometry.translate(&values).scale(scale);
                let mask = geometry.mask(&mut self.rounded_clip_pool, &mut scene, scale, viewport).scale(scale);
                Some((std::sync::Arc::new(geometry), delta, mask))
            });
            let Some((geometry, delta, mask)) = update else {
                continue;
            };
            *shared = geometry.clone();
            match &mut previous.paint_operations[index] {
                crate::scene::PaintOperation::Primitive(p) => p.translate_subtree(*delta, *mask),
                crate::scene::PaintOperation::StartLayer(bounds) => bounds.origin += *delta,
                _ => {}
            }
        }
        for hitbox in
            self.rendered_frame.hitboxes.iter_mut().chain(self.rendered_frame.window_control_hitboxes.iter_mut().map(|(_, hitbox)| hitbox))
        {
            if let Some(geometry) = &mut hitbox.subtree_geometry {
                if !geometry.translation_changed(&values) {
                    continue;
                }
                hitbox.bounds.origin += geometry.translate(&values);
                hitbox.content_mask = geometry.mask(&mut self.rounded_clip_pool, &mut scene, scale, viewport);
            }
        }
        scene.replay(0..previous.len(), &previous);
        scene.finish();
        self.rendered_frame.scene = scene;
        self.rounded_clip_pool.snapshot(&mut self.rendered_frame, scale);
        let mut a11y_deltas = std::collections::HashMap::new();
        for (id, geometry) in &mut self.subtree_a11y {
            a11y_deltas.insert(*id, geometry.translate(&values));
        }
        if let Some(update) = self.a11y.translate_subtrees(&a11y_deltas, scale) {
            self.platform_window.a11y_tree_update(update);
        }
        self.needs_present.set(true);
        self.on_next_frame(|window, _| window.needs_present.set(true));
        true
    }
    pub(crate) fn record_subtree_a11y(&mut self, id: accesskit::NodeId) {
        if let Some(geometry) = self.subtree_geometry() {
            self.subtree_a11y.insert(id, geometry);
        }
    }
}
