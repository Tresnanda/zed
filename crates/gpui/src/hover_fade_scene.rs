// Included in gpui::scene (hover_fade_gpui.py). The scene half of paint-only hover colour fades: quads painted while a
// `ColorFadeTag` is active keep it (by operation index, carried through replay like the other tags), and every present
// re-tints them in place, in the operation log, any `paintTransform` base and the sorted GPU buffer. Finished fades
// write their final colour once and stop asking for frames.

impl Scene {
    fn color_fade_slots_update(&mut self) {
        self.color_fade_slots.clear();
        if self.color_fades.is_empty() {
            return;
        }
        let mut quads = Vec::new();
        for (index, operation) in self.paint_operations.iter().enumerate() {
            if let PaintOperation::Primitive(Primitive::Quad(q)) = operation {
                quads.push((index, q.order));
            }
        }
        // `finish` sorts quads by order with a stable sort, so equal orders keep this operation order.
        quads.sort_by_key(|(_, order)| *order);
        for (slot, (index, _)) in quads.into_iter().enumerate() {
            if self.color_fades.contains_key(&index) {
                self.color_fade_slots.insert(index, slot);
            }
        }
    }

    /// Advance every hover colour fade to now. True while any still runs.
    pub(crate) fn animate_color_fades(&mut self) -> bool {
        if self.color_fades.is_empty() {
            return false;
        }
        let now = crate::window::kybern_animation_now();
        let mut running = false;
        let mut damage: Option<Bounds<ScaledPixels>> = None;
        // A finished fade writes its final colour once and leaves the map, so later scene rebuilds (every retained tick
        // replays and finishes the scene) do not map quad slots for it again.
        let mut finished = Vec::new();
        for (&index, (tag, kind)) in &self.color_fades {
            let ((background, border), active) = tag.value(now);
            if active {
                running = true;
            } else {
                finished.push(index);
                if !tag.mark_finished() { continue; }
            }
            let Some(&slot) = self.color_fade_slots.get(&index) else { continue };
            let tint = |quad: &mut Quad| {
                if *kind == 0 { quad.background = background.into() } else { quad.border_color = border }
            };
            if let Some((base, context)) = self.transform_bases.get_mut(&index) {
                let Primitive::Quad(quad) = base else { continue };
                tint(quad);
                let mut primitive = base.clone();
                primitive.transform_subtree(context);
                self.paint_operations[index] = PaintOperation::Primitive(primitive);
            } else if let PaintOperation::Primitive(Primitive::Quad(quad)) = &mut self.paint_operations[index] {
                tint(quad);
            }
            let PaintOperation::Primitive(Primitive::Quad(quad)) = &self.paint_operations[index] else { continue };
            if slot < self.quads.len() {
                let order = self.quads[slot].order;
                self.quads[slot] = *quad;
                self.quads[slot].order = order;
            }
            damage = Some(damage.map_or(quad.bounds, |d| d.union(&quad.bounds)));
        }
        for index in finished {
            self.color_fades.remove(&index);
            self.color_fade_slots.remove(&index);
        }
        if let Some(bounds) = damage {
            self.passive_paint_regions.entry(u64::MAX - 10).and_modify(|old| *old = old.union(&bounds)).or_insert(bounds);
            self.passive_paint_bounds = Some(self.passive_paint_bounds.map_or(bounds, |old| old.union(&bounds)));
        }
        running
    }
}
