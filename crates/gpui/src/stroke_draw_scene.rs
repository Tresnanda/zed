// Included in gpui::scene (stroke_draw_gpui.py). Paint-only stroke draw-in (`t-check-draw`: `stroke-dasharray: dash`,
// `stroke-dashoffset: dash -> 0`). An SVG painted inside `Window::with_stroke_draw` tags its monochrome sprite; every
// presented frame `Window::animate_strokes` points that sprite (in the operation log, the sorted GPU buffer and any
// subtree-transform base) at an atlas tile of the same SVG rasterized at the current dash offset. Geometry, order,
// clipping and hit testing never change, and no element render, layout or prepaint runs while it draws.

/// What `paintAnimation: { kind: 'stroke' }` asks for.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct StrokeDrawSpec {
    pub bytes: std::sync::Arc<[u8]>,
    pub dash: f32,
    pub delay_ms: f64,
    pub duration_ms: f64,
    /// cubic-bezier control points.
    pub ease: [f64; 4],
    /// Dash offsets the draw runs between (`dash` -> 0 by default: a draw-in). With `dasharray: dash` on a closed path of
    /// length `dash`, offset `dash * (1 - v)` shows the fraction `v` from the path start (a ring's arc).
    pub from: f32,
    pub to: f32,
    /// A new `to` on the same glyph eases from the offset on screen instead of restarting (N2: ring values).
    pub retarget: bool,
}
impl StrokeDrawSpec {
    pub(crate) fn same(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.bytes, &other.bytes) || *self.bytes == *other.bytes
    }
    fn same_timing(&self, other: &Self) -> bool {
        self.dash == other.dash && self.delay_ms == other.delay_ms && self.duration_ms == other.duration_ms && self.ease == other.ease
            && self.from == other.from && self.to == other.to
    }
}

/// One drawing glyph: shared by every scene that holds its sprite, so its clock and tiles survive re-renders.
pub(crate) struct StrokeDraw {
    /// As declared (what a later paint compares against).
    pub(crate) spec: StrokeDrawSpec,
    /// Where this run starts (the declared `from`, or the offset on screen when it retargeted) and its delay.
    pub(crate) from: f32,
    pub(crate) delay_ms: f64,
    pub(crate) started: std::time::Instant,
    pub(crate) serial: u64,
    pub(crate) size: crate::Size<crate::DevicePixels>,
    pub(crate) state: std::sync::Mutex<StrokeDrawState>,
}

#[derive(Default)]
pub(crate) struct StrokeDrawState {
    /// The glyph as painted (the whole stroke): what the sprite shows once the draw has ended.
    pub(crate) whole: Option<AtlasTile>,
    /// Tiles rasterized so far, by quantized dash offset (1/64 of a user unit).
    pub(crate) tiles: Vec<(i32, crate::AtlasKey, AtlasTile)>,
    /// 0: drawing; 1: ended, intermediate tiles still allocated for one more frame; 2: ended and released.
    pub(crate) phase: u8,
    pub(crate) last_seen: Option<std::time::Instant>,
}

impl StrokeDraw {
    pub(crate) fn new(spec: StrokeDrawSpec, size: crate::Size<crate::DevicePixels>, started: std::time::Instant) -> Self {
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            from: spec.from,
            delay_ms: spec.delay_ms,
            spec,
            started,
            serial: SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            size,
            state: Default::default(),
        }
    }
    pub(crate) fn reusable(&self, spec: &StrokeDrawSpec, size: crate::Size<crate::DevicePixels>) -> bool {
        self.size == size && self.spec.same(spec) && self.spec.same_timing(spec)
    }
    /// Dash offset at `now` (`from` before the delay, `to` at the end) and whether the curve has ended.
    pub(crate) fn offset_at(&self, now: std::time::Instant) -> (f32, bool) {
        let elapsed = now.saturating_duration_since(self.started).as_secs_f64() * 1000.0 - self.delay_ms;
        // A 1 ms draw is how Reduce Motion asks for the end at once, even on a paused clock.
        let x = if self.spec.duration_ms <= 1.0 { 1.0 } else { (elapsed / self.spec.duration_ms).clamp(0.0, 1.0) };
        let [x1, y1, x2, y2] = self.spec.ease;
        let at = |t: f64, a: f64, b: f64| 3.0 * a * t * (1.0 - t).powi(2) + 3.0 * b * t * t * (1.0 - t) + t * t * t;
        let (mut lo, mut hi) = (0.0f64, 1.0f64);
        for _ in 0..28 {
            let mid = (lo + hi) * 0.5;
            if at(mid, x1, x2) < x { lo = mid } else { hi = mid }
        }
        let e = if x <= 0.0 { 0.0 } else if x >= 1.0 { 1.0 } else { at((lo + hi) * 0.5, y1, y2) };
        (self.from + (self.spec.to - self.from) * e as f32, x >= 1.0 && elapsed >= 0.0)
    }
    /// The SVG source with every path dashed at `offset`.
    pub(crate) fn dashed(&self, offset: f32) -> Vec<u8> {
        let text = String::from_utf8_lossy(&self.spec.bytes);
        let attrs = format!(" stroke-dasharray=\"{}\" stroke-dashoffset=\"{:.4}\" ", self.spec.dash, offset);
        text.replace("<path ", &format!("<path{attrs}")).replace("<circle ", &format!("<circle{attrs}")).into_bytes()
    }
    pub(crate) fn key(&self, q: i32) -> crate::AtlasKey {
        crate::AtlasKey::Svg(crate::RenderSvgParams {
            path: format!("__kybern_stroke_{}_{}", self.serial, q).into(),
            size: self.size,
        })
    }
}

impl Scene {
    /// Tag a monochrome sprite painted inside `with_stroke_draw` (or replayed with its tag).
    fn stroke_tag_for(&mut self, primitive: &Primitive) -> Option<std::sync::Arc<StrokeDraw>> {
        if let Some(tag) = self.pending_stroke.take() {
            return Some(tag);
        }
        let Primitive::MonochromeSprite(sprite) = primitive else { return None };
        let draw = self.active_stroke.clone()?;
        let mut state = draw.state.lock().unwrap();
        if !state.tiles.iter().any(|(_, _, tile)| tile.tile_id == sprite.tile.tile_id && tile.texture_id == sprite.tile.texture_id) {
            state.whole = Some(sprite.tile);
        }
        drop(state);
        Some(draw)
    }
    fn stroke_slots_update(&mut self) {
        self.stroke_slots.clear();
        if self.stroke_tags.is_empty() {
            return;
        }
        let mut mono = Vec::new();
        for (index, operation) in self.paint_operations.iter().enumerate() {
            if let PaintOperation::Primitive(Primitive::MonochromeSprite(p)) = operation {
                mono.push((index, (p.order, p.tile.tile_id)));
            }
        }
        mono.sort_by_key(|(_, key)| *key);
        for (slot, (index, _)) in mono.into_iter().enumerate() {
            if self.stroke_tags.contains_key(&index) {
                self.stroke_slots.insert(index, slot);
            }
        }
    }
    /// Point every tagged sprite of `draw` at `tile`, in place. Returns the damaged bounds.
    pub(crate) fn set_stroke_tile(&mut self, index: usize, tile: AtlasTile) -> Option<Bounds<ScaledPixels>> {
        let slot = self.stroke_slots.get(&index).copied();
        let PaintOperation::Primitive(Primitive::MonochromeSprite(p)) = &mut self.paint_operations[index] else { return None };
        if p.tile != tile {
            p.tile = tile;
            if let Some(sprite) = slot.and_then(|slot| self.monochrome_sprites.get_mut(slot)) {
                sprite.tile = tile;
            }
            if let Some((Primitive::MonochromeSprite(base), _)) = self.transform_bases.get_mut(&index) {
                base.tile = tile;
            }
        }
        Some(p.bounds.intersect(&p.content_mask.bounds))
    }
}
