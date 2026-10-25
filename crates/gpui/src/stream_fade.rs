// Included in gpui::scene (stream_fade_gpui.py). Paint-only per-word fade-in (`.t-stream-w`: opacity
// 0 -> 1 and `filter: blur(1px)` -> 0 over `--stream-fade`, `--ease-smooth-out`). Glyph sprites whose
// centre lies in a fading word are tagged as they enter the scene; each presented frame then sets their
// alpha (and the word's blur pass) in place, in both the operation log and the sorted GPU buffers. No
// element render, layout or prepaint runs between text updates, and finished words drop their tags.

/// A word that is fading in.
#[derive(Clone, Copy, Debug)]
#[allow(missing_docs)]
pub struct FadeWord {
    pub bounds: Bounds<ScaledPixels>,
    pub logical: Bounds<crate::Pixels>,
    pub born: std::time::Instant,
    pub fade_ms: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FadeTag {
    born: std::time::Instant,
    fade_ms: f32,
    base: Hsla,
    /// Device-pixel blur at the start of the fade; zero for glyphs.
    blur: f32,
}
impl FadeTag {
    pub(crate) fn new(born: std::time::Instant, fade_ms: f32, blur: f32) -> Self {
        Self { born, fade_ms, base: Hsla::default(), blur }
    }
    pub(crate) fn progress(&self, now: std::time::Instant) -> f32 {
        let x = (now.saturating_duration_since(self.born).as_secs_f64() * 1000.0 / self.fade_ms.max(1.0) as f64).clamp(0.0, 1.0);
        // `--ease-smooth-out`: cubic-bezier(0.22, 1, 0.36, 1).
        let at = |t: f64, a: f64, b: f64| 3.0 * a * t * (1.0 - t).powi(2) + 3.0 * b * t * t * (1.0 - t) + t * t * t;
        let (mut lo, mut hi) = (0.0f64, 1.0f64);
        for _ in 0..24 {
            let mid = (lo + hi) * 0.5;
            if at(mid, 0.22, 0.36) < x { lo = mid } else { hi = mid }
        }
        at((lo + hi) * 0.5, 1.0, 1.0) as f32
    }
    pub(crate) fn done(&self, now: std::time::Instant) -> bool {
        now.saturating_duration_since(self.born).as_secs_f64() * 1000.0 >= self.fade_ms as f64
    }
    /// Blur of a word's pass at `progress`; the pass is skipped once it is invisible.
    pub(crate) fn blur_at(&self, progress: f32) -> (f32, f32) {
        let blur = self.blur * (1.0 - progress);
        (blur, if blur > 0.05 { 1.0 } else { 0.0 })
    }
}

impl Scene {
    /// Tag (and fade to the current phase) a glyph sprite that belongs to a fading word.
    fn fade_tag_for(&mut self, primitive: &mut Primitive) -> Option<FadeTag> {
        if let Some(tag) = self.pending_fade.take() {
            return Some(tag);
        }
        let words = self.active_fade.as_ref()?;
        let (bounds, color) = match primitive {
            Primitive::MonochromeSprite(p) => (p.bounds, &mut p.color),
            Primitive::SubpixelSprite(p) => (p.bounds, &mut p.color),
            _ => return None,
        };
        let centre = Point::new(
            ScaledPixels(bounds.origin.x.0 + bounds.size.width.0 / 2.0),
            ScaledPixels(bounds.origin.y.0 + bounds.size.height.0 / 2.0),
        );
        let word = words.iter().find(|w| w.bounds.contains(&centre))?;
        let tag = FadeTag { born: word.born, fade_ms: word.fade_ms, base: *color, blur: 0.0 };
        let now = std::time::Instant::now();
        if tag.done(now) {
            return None;
        }
        color.a = tag.base.a * tag.progress(now);
        Some(tag)
    }
    fn fade_slots_update(&mut self) {
        self.fade_slots.clear();
        if self.fade_tags.is_empty() {
            return;
        }
        let (mut mono, mut subpixel, mut backdrops) = (Vec::new(), Vec::new(), Vec::new());
        for (index, operation) in self.paint_operations.iter().enumerate() {
            match operation {
                PaintOperation::Primitive(Primitive::MonochromeSprite(p)) => mono.push((index, (p.order, p.tile.tile_id))),
                PaintOperation::Primitive(Primitive::SubpixelSprite(p)) => subpixel.push((index, (p.order, p.tile.tile_id))),
                PaintOperation::Primitive(Primitive::Backdrop(p)) => backdrops.push((index, (p.order, crate::TileId(0)))),
                _ => {}
            }
        }
        for (kind, mut list) in [(1u8, mono), (3, subpixel), (4, backdrops)] {
            list.sort_by_key(|(_, key)| *key);
            for (slot, (index, _)) in list.into_iter().enumerate() {
                if self.fade_tags.contains_key(&index) {
                    self.fade_slots.insert(index, (kind, slot));
                }
            }
        }
    }
    /// Advance every fading word to now. True while any is still fading.
    pub(crate) fn animate_fades(&mut self) -> bool {
        if self.fade_tags.is_empty() {
            return false;
        }
        let now = std::time::Instant::now();
        let mut finished = Vec::new();
        let mut fading = false;
        let mut damage: Option<Bounds<ScaledPixels>> = None;
        for (&index, tag) in &self.fade_tags {
            let Some(&(kind, slot)) = self.fade_slots.get(&index) else {
                finished.push(index);
                continue;
            };
            let progress = tag.progress(now);
            let bounds = match (&mut self.paint_operations[index], kind) {
                (PaintOperation::Primitive(Primitive::MonochromeSprite(p)), 1) if slot < self.monochrome_sprites.len() => {
                    p.color.a = tag.base.a * progress;
                    self.monochrome_sprites[slot].color = p.color;
                    p.bounds
                }
                (PaintOperation::Primitive(Primitive::SubpixelSprite(p)), 3) if slot < self.subpixel_sprites.len() => {
                    p.color.a = tag.base.a * progress;
                    self.subpixel_sprites[slot].color = p.color;
                    p.bounds
                }
                (PaintOperation::Primitive(Primitive::Backdrop(p)), 4) if slot < self.backdrops.len() => {
                    let (blur, opacity) = tag.blur_at(progress);
                    p.blur = ScaledPixels(blur);
                    p.opacity = opacity;
                    self.backdrops[slot].blur = p.blur;
                    self.backdrops[slot].opacity = opacity;
                    p.bounds
                }
                _ => {
                    finished.push(index);
                    continue;
                }
            };
            damage = Some(damage.map_or(bounds, |d| d.union(&bounds)));
            if tag.done(now) { finished.push(index) } else { fading = true }
        }
        for index in finished {
            self.fade_tags.remove(&index);
            self.fade_slots.remove(&index);
        }
        if let Some(bounds) = damage {
            self.passive_paint_regions.entry(u64::MAX - 9).and_modify(|old| *old = old.union(&bounds)).or_insert(bounds);
            self.passive_paint_bounds = Some(self.passive_paint_bounds.map_or(bounds, |old| old.union(&bounds)));
        }
        fading
    }
    /// A pop that finished while no frames were presented (a hidden window) still lands on its last
    /// keyframe: scale 1 and full opacity, applied once in place.
    fn finish_pops(&mut self) {
        let unit = TransformationMatrix::unit();
        for (&index, &(_, _, inherited_opacity, animation, color)) in &self.repaint_tags {
            if !animation.is_some_and(|a| a.pop && a.pop_done(paint_animation_clock(a))) { continue }
            let Some(&(kind, slot)) = self.repaint_slots.get(&index) else { continue };
            let color = color.opacity(inherited_opacity);
            match (&mut self.paint_operations[index], kind) {
                (PaintOperation::Primitive(Primitive::MonochromeSprite(p)), 1 | 2) if slot < self.monochrome_sprites.len() => {
                    if p.transformation != unit || p.color != color {
                        p.transformation = unit;
                        p.color = color;
                        self.monochrome_sprites[slot].transformation = unit;
                        self.monochrome_sprites[slot].color = color;
                    }
                }
                (PaintOperation::Primitive(Primitive::SubpixelSprite(p)), 3) if slot < self.subpixel_sprites.len() => {
                    if p.transformation != unit || p.color != color {
                        p.transformation = unit;
                        p.color = color;
                        self.subpixel_sprites[slot].transformation = unit;
                        self.subpixel_sprites[slot].color = color;
                    }
                }
                _ => {}
            }
        }
    }
}
