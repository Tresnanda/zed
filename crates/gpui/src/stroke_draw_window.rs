// Included in gpui::window (stroke_draw_gpui.py): the paint and per-frame sides of stroke_draw_scene.rs.
static STROKE_DRAW_CLOCK: std::sync::RwLock<Option<std::sync::Arc<dyn Fn() -> std::time::Instant + Send + Sync>>> = std::sync::RwLock::new(None);

/// The clock stroke draws read: a host's automation clock (which tests can pause and step), or the real clock.
pub fn set_stroke_draw_clock(clock: Option<std::sync::Arc<dyn Fn() -> std::time::Instant + Send + Sync>>) {
    *STROKE_DRAW_CLOCK.write().unwrap() = clock;
}
fn stroke_draw_now() -> std::time::Instant {
    STROKE_DRAW_CLOCK.read().unwrap().as_ref().map_or_else(std::time::Instant::now, |clock| clock())
}

impl Window {
    /// Paint an SVG whose paths stroke themselves in (`spec`), starting the first time `id` paints. Later paints of the
    /// same glyph keep its clock, so re-renders never restart the draw.
    pub fn with_stroke_draw<R>(
        &mut self,
        id: u64,
        spec: &crate::StrokeDrawSpec,
        bounds: Bounds<Pixels>,
        cx: &mut App,
        paint: impl FnOnce(&mut Self, &mut App) -> R,
    ) -> R {
        let snapped = self.snap_bounds(bounds);
        let size = snapped.size.map(|v| DevicePixels::from((v.0 * crate::SMOOTH_SVG_SCALE_FACTOR).ceil() as i32));
        let draw = match self.stroke_draws.get(&id) {
            Some(draw) if draw.reusable(spec, size) => draw.clone(),
            _ => {
                let now = stroke_draw_now();
                let mut next = crate::scene::StrokeDraw::new(spec.clone(), size, now);
                // A ring's new value eases from what it shows now, with no delay (`retarget`).
                if let Some(old) = self.stroke_draws.get(&id).filter(|old| spec.retarget && old.size == size && old.spec.same(spec)) {
                    next.from = old.offset_at(now).0;
                    next.delay_ms = 0.0;
                }
                let draw = std::sync::Arc::new(next);
                if let Some(old) = self.stroke_draws.insert(id, draw.clone()) {
                    self.release_stroke_tiles(&old);
                }
                draw
            }
        };
        if self.stroke_svg_renderer.is_none() {
            self.stroke_svg_renderer = Some(cx.svg_renderer.clone());
        }
        let previous = self.next_frame.scene.active_stroke.replace(draw);
        let result = paint(self, cx);
        self.next_frame.scene.active_stroke = previous;
        result
    }

    fn release_stroke_tiles(&self, draw: &crate::scene::StrokeDraw) {
        let mut state = draw.state.lock().unwrap();
        for (_, key, _) in state.tiles.drain(..) {
            self.sprite_atlas.remove(&key);
        }
    }

    /// Advance every drawing glyph of the rendered scene to now, before it is presented. True while one still draws.
    pub(crate) fn animate_strokes(&mut self) -> bool {
        if self.stroke_draws.is_empty() {
            return false;
        }
        let now = stroke_draw_now();
        let tagged: Vec<(usize, std::sync::Arc<crate::scene::StrokeDraw>)> =
            self.rendered_frame.scene.stroke_tags.iter().map(|(&index, draw)| (index, draw.clone())).collect();
        let mut drawing = false;
        let mut damage: Option<Bounds<ScaledPixels>> = None;
        let mut tiles: std::collections::HashMap<u64, Option<AtlasTile>> = Default::default();
        for (index, draw) in tagged {
            let tile = *tiles.entry(draw.serial).or_insert_with(|| {
                let (tile, still) = self.stroke_tile(&draw, now);
                drawing |= still;
                tile
            });
            let Some(tile) = tile else { continue };
            if let Some(bounds) = self.rendered_frame.scene.set_stroke_tile(index, tile) {
                damage = Some(damage.map_or(bounds, |d| d.union(&bounds)));
            }
        }
        if let Some(bounds) = damage {
            let scene = &mut self.rendered_frame.scene;
            scene.passive_paint_regions.entry(u64::MAX - 10).and_modify(|old| *old = old.union(&bounds)).or_insert(bounds);
            scene.passive_paint_bounds = Some(scene.passive_paint_bounds.map_or(bounds, |old| old.union(&bounds)));
        }
        // Forget glyphs that have not been painted for two seconds (unmounted), releasing any tiles they still hold.
        let stale: Vec<u64> = self
            .stroke_draws
            .iter()
            .filter(|(_, draw)| {
                let state = draw.state.lock().unwrap();
                state.last_seen.is_none_or(|seen| now.saturating_duration_since(seen).as_secs_f64() > 2.0)
                    && now.saturating_duration_since(draw.started).as_secs_f64() > 2.0
            })
            .map(|(&id, _)| id)
            .collect();
        for id in stale {
            if let Some(draw) = self.stroke_draws.remove(&id) {
                self.release_stroke_tiles(&draw);
            }
        }
        drawing
    }

    /// The tile `draw` shows at `now` (None: leave the sprite as painted) and whether it still needs frames.
    fn stroke_tile(&mut self, draw: &crate::scene::StrokeDraw, now: std::time::Instant) -> (Option<AtlasTile>, bool) {
        let (offset, ended) = draw.offset_at(now);
        let mut state = draw.state.lock().unwrap();
        state.last_seen = Some(now);
        // A draw that rests part-way (a ring's value) keeps showing its `to` tile; the plain glyph is the whole ring.
        let rests = draw.spec.to.abs() > 1e-3;
        if ended && rests && state.phase >= 1 {
            let q = (draw.spec.to * 64.0).round() as i32;
            if state.phase == 1 {
                state.phase = 2;
                let keep: Vec<_> = state.tiles.drain(..).collect();
                for (at, key, tile) in keep {
                    if at == q { state.tiles.push((at, key, tile)) } else { self.sprite_atlas.remove(&key) }
                }
            }
            if let Some((_, _, tile)) = state.tiles.iter().find(|(at, _, _)| *at == q) {
                return (Some(*tile), false);
            }
        } else if ended && rests {
            state.phase = 1;
        } else if ended {
            let whole = state.whole;
            match state.phase {
                // One more presented frame on the whole glyph before the intermediate tiles go back to the atlas.
                0 => { state.phase = 1; return (whole, true) }
                1 => {
                    state.phase = 2;
                    for (_, key, _) in state.tiles.drain(..) {
                        self.sprite_atlas.remove(&key);
                    }
                    return (whole, false);
                }
                _ => return (whole, false),
            }
        }
        let q = (offset * 64.0).round() as i32;
        if let Some((_, _, tile)) = state.tiles.iter().find(|(at, _, _)| *at == q) {
            return (Some(*tile), true);
        }
        let Some(renderer) = self.stroke_svg_renderer.clone() else { return (None, true) };
        let key = draw.key(q);
        let crate::AtlasKey::Svg(params) = &key else { unreachable!() };
        let bytes = draw.dashed(q as f32 / 64.0);
        let tile = self.sprite_atlas.get_or_insert_with(&key, &mut || {
            Ok(renderer.render_alpha_mask(params, Some(&bytes))?.map(|(size, data)| (size, std::borrow::Cow::Owned(data))))
        });
        match tile {
            Ok(Some(tile)) => {
                state.tiles.push((q, key, tile));
                (Some(tile), true)
            }
            _ => (None, true),
        }
    }
}
