// Included in gpui::window (subtree_transform_gpui.py). A paint-only scale / rotation /
// translation / opacity / blur about an origin, applied to every primitive painted inside a
// scope as it enters the scene. The untransformed primitive is kept beside it, so retained
// updates recompute each frame from that base: no render, layout, prepaint, island wake or
// ancestor invalidation. Hitboxes, rounded masks established inside the scope and AccessKit
// bounds follow; ancestor masks stay in window coordinates. Layout slots never move.
//
// Rotation is exact for glyph and SVG sprites and for paths. Quads, shadows, images, surfaces
// and masks are axis-aligned primitives: under rotation their centres rotate and their size
// scales, and hitboxes/masks use the axis-aligned envelope of the rotated rectangle.

/// One scope's current value. Angles are clockwise degrees; blur is logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(missing_docs)]
pub struct SubtreeTransformValue {
    pub scale: f32,
    /// One-axis multipliers of `scale` (N5: a switch thumb's held stretch, a bar filling from one end).
    pub scale_x: f32,
    pub scale_y: f32,
    /// Multiplies only the subtree's drop shadows (N8: a card's lift shadow fades in as the card rises).
    pub shadow_opacity: f32,
    pub rotate: f32,
    pub translate_x: f32,
    pub translate_y: f32,
    pub opacity: f32,
    pub blur: f32,
    /// A rounded clip over the subtree (an image morphing out of a cropped thumbnail): the host's layout box inset
    /// by `clip` (top, right, bottom, left; logical pixels, before the transform), mapped through the scope, with
    /// corners of `clip_radius` logical pixels on screen whatever the scale. Negative `clip_radius`: no clip.
    pub clip: [f32; 4],
    pub clip_radius: f32,
}
impl Default for SubtreeTransformValue {
    fn default() -> Self {
        Self {
            scale: 1.0,
            scale_x: 1.0,
            scale_y: 1.0,
            shadow_opacity: 1.0,
            rotate: 0.0,
            translate_x: 0.0,
            translate_y: 0.0,
            opacity: 1.0,
            blur: 0.0,
            clip: [0.0; 4],
            clip_radius: -1.0,
        }
    }
}
impl SubtreeTransformValue {
    #[allow(missing_docs)]
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }
}

/// `x' = a·x + b·y + tx`, `y' = c·x + d·y + ty`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Affine {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    tx: f32,
    ty: f32,
}
impl Affine {
    const IDENTITY: Self = Self { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: 0.0 };
    /// `T(origin + translate) · R(rotate) · S(scale · scaleX, scale · scaleY) · T(-origin)`.
    fn about(value: &SubtreeTransformValue, origin: Point<Pixels>) -> Self {
        let (sin, cos) = value.rotate.to_radians().sin_cos();
        let (sx, sy) = (value.scale * value.scale_x, value.scale * value.scale_y);
        let (a, b, c, d) = (cos * sx, -sin * sy, sin * sx, cos * sy);
        let (ox, oy) = (origin.x.as_f32(), origin.y.as_f32());
        Self { a, b, c, d, tx: ox + value.translate_x - a * ox - b * oy, ty: oy + value.translate_y - c * ox - d * oy }
    }
    /// `self ∘ inner`: apply `inner` first.
    fn then(self, inner: Self) -> Self {
        Self {
            a: self.a * inner.a + self.b * inner.c,
            b: self.a * inner.b + self.b * inner.d,
            c: self.c * inner.a + self.d * inner.c,
            d: self.c * inner.b + self.d * inner.d,
            tx: self.a * inner.tx + self.b * inner.ty + self.tx,
            ty: self.c * inner.tx + self.d * inner.ty + self.ty,
        }
    }
    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (self.a * x + self.b * y + self.tx, self.c * x + self.d * y + self.ty)
    }
    /// Uniform scale magnitude.
    fn magnitude(&self) -> f32 {
        (self.a * self.d - self.b * self.c).abs().sqrt()
    }
    /// The same map in device pixels.
    fn scaled(self, factor: f32) -> Self {
        Self { tx: self.tx * factor, ty: self.ty * factor, ..self }
    }
    /// Axis-aligned envelope of a transformed rectangle.
    fn envelope(&self, x: f32, y: f32, w: f32, h: f32) -> (f32, f32, f32, f32) {
        let corners = [self.apply(x, y), self.apply(x + w, y), self.apply(x, y + h), self.apply(x + w, y + h)];
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (cx, cy) in corners {
            x0 = x0.min(cx);
            y0 = y0.min(cy);
            x1 = x1.max(cx);
            y1 = y1.max(cy);
        }
        (x0, y0, x1 - x0, y1 - y0)
    }
    /// How much the map stretches each axis (its column lengths); equal for a uniform scale.
    fn axes(&self) -> (f32, f32) {
        ((self.a * self.a + self.c * self.c).sqrt(), (self.b * self.b + self.d * self.d).sqrt())
    }
    /// A rectangle that keeps its axes: its centre follows the map, its width and height the axis stretches.
    fn rect(&self, x: f32, y: f32, w: f32, h: f32) -> (f32, f32, f32, f32) {
        let (sx, sy) = self.axes();
        let (cx, cy) = self.apply(x + w / 2.0, y + h / 2.0);
        (cx - w * sx / 2.0, cy - h * sy / 2.0, w * sx, h * sy)
    }
}

/// Everything a primitive painted in one scope / mask context needs. Immutable and shared by
/// every primitive of that context; a retained update builds one replacement per context.
#[derive(Clone, Debug)]
pub struct SubtreeTransformContext {
    scopes: Vec<(u64, Point<Pixels>, SubtreeTransformValue)>,
    /// Each scope's host layout box (its `clip` is inset from it).
    boxes: Vec<Bounds<Pixels>>,
    outer: ContentMask<Pixels>,
    masks: Vec<(Bounds<Pixels>, Option<crate::RoundedClip>, usize)>,
    matrix: Affine,
    opacity: f32,
    shadow_opacity: f32,
    blur: f32,
    scale_factor: f32,
    visual_mask: ContentMask<Pixels>,
    /// `visual_mask` before any transform: what `content_mask()` reads while painting in this context. A primitive's own
    /// clip narrower than this is its own; the rest of it is the ancestors' masks, already in `visual_mask`.
    base_mask: Bounds<Pixels>,
}
impl SubtreeTransformContext {
    fn build(
        scopes: Vec<(u64, Point<Pixels>, SubtreeTransformValue)>,
        boxes: Vec<Bounds<Pixels>>,
        outer: ContentMask<Pixels>,
        masks: Vec<(Bounds<Pixels>, Option<crate::RoundedClip>, usize)>,
        scale_factor: f32,
        pool: &mut RoundedClipPool,
        scene: &mut Scene,
    ) -> Self {
        let compose = |depth: usize| scopes[..depth].iter().fold(Affine::IDENTITY, |m, (_, origin, value)| m.then(Affine::about(value, *origin)));
        let matrix = compose(scopes.len());
        let opacity = scopes.iter().map(|(_, _, v)| v.opacity.clamp(0.0, 1.0)).product();
        let shadow_opacity = scopes.iter().map(|(_, _, v)| v.shadow_opacity.clamp(0.0, 1.0)).product();
        let blur = scopes.last().map_or(0.0, |(_, _, v)| v.blur.max(0.0));
        // The mask at the outermost scope stays put; masks established inside follow the
        // scopes that were active when they were pushed.
        let mut mask = outer.clone();
        let base_mask = masks.iter().fold(outer.bounds, |base, (bounds, _, _)| base.intersect(bounds));
        // A scope's own clip, in its host box mapped through every scope up to and including it; its corners are
        // given on screen, so a thumbnail's radius holds while the image scales up.
        for (depth, ((_, _, value), host)) in scopes.iter().zip(&boxes).enumerate() {
            if value.clip_radius < 0.0 {
                continue;
            }
            let [top, right, bottom, left] = value.clip;
            let (w, h) = ((host.size.width.as_f32() - left - right).max(0.0), (host.size.height.as_f32() - top - bottom).max(0.0));
            let (rx, ry, rw, rh) = compose(depth + 1).rect(host.origin.x.as_f32() + left, host.origin.y.as_f32() + top, w, h);
            let visual = Bounds::new(point(px(rx), px(ry)), size(px(rw), px(rh)));
            let r = value.clip_radius.min(rw.min(rh) / 2.0);
            if r > 0.0 {
                let (id, generation) = pool.intern(visual, Corners::all(px(r)), mask.clone());
                pool.sync_node(scene, id, scale_factor);
                mask.rounded_clip = id;
                mask.clip_generation = generation;
            }
            mask.bounds = mask.bounds.intersect(&visual);
        }
        for (bounds, rounded, depth) in &masks {
            let m = compose(*depth);
            let (x, y, w, h) = m.envelope(bounds.origin.x.as_f32(), bounds.origin.y.as_f32(), bounds.size.width.as_f32(), bounds.size.height.as_f32());
            let visual = Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
            if let Some(shape) = rounded {
                let (sx, sy) = m.axes();
                let s = sx.min(sy);
                let (rx, ry, rw, rh) = m.rect(shape.bounds[0], shape.bounds[1], shape.bounds[2], shape.bounds[3]);
                // Radii, or an edge fade's ramps, scale with the scope.
                let (id, generation) =
                    pool.intern_node(Bounds::new(point(px(rx), px(ry)), size(px(rw), px(rh))), shape.radii.map(|r| r * s), mask.clone(), shape.kind);
                pool.sync_node(scene, id, scale_factor);
                mask.rounded_clip = id;
                mask.clip_generation = generation;
            }
            mask.bounds = mask.bounds.intersect(&visual);
        }
        Self { scopes, boxes, outer, masks, matrix, opacity, shadow_opacity, blur, scale_factor, visual_mask: mask, base_mask }
    }
    /// A replacement when any of `values` changes one of this context's scopes.
    fn updated(
        &self,
        values: &std::collections::HashMap<u64, SubtreeTransformValue>,
        pool: &mut RoundedClipPool,
        scene: &mut Scene,
    ) -> Option<Self> {
        if !self.scopes.iter().any(|(id, _, old)| values.get(id).is_some_and(|next| next != old)) {
            return None;
        }
        let scopes = self.scopes.iter().map(|(id, origin, old)| (*id, *origin, values.get(id).copied().unwrap_or(*old))).collect();
        Some(Self::build(scopes, self.boxes.clone(), self.outer.clone(), self.masks.clone(), self.scale_factor, pool, scene))
    }
    fn contains(&self, id: u64) -> bool {
        self.scopes.iter().any(|(scope, _, _)| *scope == id)
    }
    fn visual_bounds(&self, bounds: Bounds<Pixels>) -> Bounds<Pixels> {
        let (x, y, w, h) = self.matrix.envelope(bounds.origin.x.as_f32(), bounds.origin.y.as_f32(), bounds.size.width.as_f32(), bounds.size.height.as_f32());
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }
}

#[derive(Default)]
struct SubtreeTransformState {
    scopes: Vec<(u64, Point<Pixels>, SubtreeTransformValue, ContentMask<Pixels>, Bounds<Pixels>)>,
    masks: Vec<Option<(Bounds<Pixels>, Option<crate::RoundedClip>, usize)>>,
    context: Option<std::sync::Arc<SubtreeTransformContext>>,
    a11y: std::collections::HashMap<accesskit::NodeId, (Bounds<Pixels>, std::sync::Arc<SubtreeTransformContext>)>,
}

impl Window {
    /// Paint `f` inside a transform scope about `origin` (logical window coordinates of the
    /// untransformed subtree). Layout is untouched. `id` addresses the scope in
    /// `transform_subtrees`; `bounds` is the host's layout box (`value.clip` insets it). Use it in both
    /// prepaint (hitboxes) and paint.
    pub fn with_subtree_transform<R>(
        &mut self,
        id: u64,
        origin: Point<Pixels>,
        bounds: Bounds<Pixels>,
        value: SubtreeTransformValue,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let outer = self.content_mask();
        self.next_frame.scene.transform_scopes.insert(id);
        self.subtree_transform.scopes.push((id, origin, value, outer, bounds));
        self.sync_subtree_transform();
        let result = f(self);
        self.subtree_transform.scopes.pop();
        self.sync_subtree_transform();
        result
    }
    fn sync_subtree_transform(&mut self) {
        let scale_factor = self.scale_factor();
        let state = &self.subtree_transform;
        let context = (!state.scopes.is_empty()).then(|| {
            let scopes = state.scopes.iter().map(|(id, origin, value, _, _)| (*id, *origin, *value)).collect();
            let boxes = state.scopes.iter().map(|(_, _, _, _, bounds)| *bounds).collect();
            let masks = state.masks.iter().flatten().filter(|(_, _, depth)| *depth > 0).cloned().collect();
            let outer = state.scopes[0].3.clone();
            std::sync::Arc::new(SubtreeTransformContext::build(
                scopes,
                boxes,
                outer,
                masks,
                scale_factor,
                &mut self.rounded_clip_pool,
                &mut self.next_frame.scene,
            ))
        });
        self.subtree_transform.context = context.clone();
        self.next_frame.scene.active_transform = context;
    }
    fn push_transform_mask(&mut self, mask: ContentMask<Pixels>) {
        let depth = self.subtree_transform.scopes.len();
        if depth == 0 {
            self.subtree_transform.masks.push(None);
            return;
        }
        let inherited = self.content_mask().rounded_clip;
        let rounded =
            (mask.rounded_clip != 0 && mask.rounded_clip != inherited).then(|| self.rounded_clip_pool.nodes[mask.rounded_clip as usize]);
        self.subtree_transform.masks.push(Some((mask.bounds, rounded, depth)));
        self.sync_subtree_transform();
    }
    fn pop_transform_mask(&mut self) {
        if self.subtree_transform.masks.pop().flatten().is_some() {
            self.sync_subtree_transform();
        }
    }
    fn transform_hitbox(&self, mut hitbox: Hitbox) -> Hitbox {
        if let Some(context) = &self.subtree_transform.context {
            hitbox.transform_base = Some((hitbox.bounds, context.clone()));
            hitbox.bounds = context.visual_bounds(hitbox.bounds);
            hitbox.content_mask = context.visual_mask.clone();
        }
        hitbox
    }
    pub(crate) fn transform_a11y_node(&mut self, id: accesskit::NodeId, node: &mut accesskit::Node) {
        let Some(context) = self.subtree_transform.context.clone() else {
            self.subtree_transform.a11y.remove(&id);
            return;
        };
        let Some(base) = self.a11y.node_bounds.get(&id).copied() else { return };
        let visual = context.visual_bounds(base);
        self.a11y.node_bounds.insert(id, visual);
        node.set_bounds(a11y_rect(visual, self.scale_factor()));
        self.subtree_transform.a11y.insert(id, (base, context));
    }
    /// Whether the rendered scene painted scope `id` (it may have drawn nothing: then a retained update has nothing to move).
    pub fn has_subtree_transform(&self, id: u64) -> bool {
        self.rendered_frame.scene.transform_scopes.contains(&id) || self.rendered_frame.scene.transform_bases.values().any(|(_, context)| context.contains(id))
    }
    /// Re-transform retained paint, hitboxes and accessibility bounds of the given scopes
    /// from their untransformed bases. Scopes without retained paint are ignored; callers
    /// check `has_subtree_transform` and render those normally.
    pub fn transform_subtrees(&mut self, updates: &[(u64, SubtreeTransformValue)]) -> bool {
        if !self.invalidator.not_drawing() {
            return false;
        }
        let values: std::collections::HashMap<_, _> = updates.iter().copied().collect();
        let scale = self.scale_factor();
        let mut previous = std::mem::take(&mut self.rendered_frame.scene);
        let mut scene = Scene::default();
        self.rounded_clip_pool.seed(&mut scene, scale);
        let mut replaced: std::collections::HashMap<usize, Option<std::sync::Arc<SubtreeTransformContext>>> = Default::default();
        let mut changed = false;
        for (&index, (base, context)) in previous.transform_bases.iter_mut() {
            let key = std::sync::Arc::as_ptr(context) as usize;
            let next = replaced
                .entry(key)
                .or_insert_with(|| context.updated(&values, &mut self.rounded_clip_pool, &mut scene).map(std::sync::Arc::new));
            let Some(next) = next else { continue };
            *context = next.clone();
            let mut primitive = base.clone();
            primitive.transform_subtree(next);
            previous.paint_operations[index] = crate::scene::PaintOperation::Primitive(primitive);
            changed = true;
        }
        for hitbox in
            self.rendered_frame.hitboxes.iter_mut().chain(self.rendered_frame.window_control_hitboxes.iter_mut().map(|(_, hitbox)| hitbox))
        {
            let Some((base, context)) = &mut hitbox.transform_base else { continue };
            let key = std::sync::Arc::as_ptr(context) as usize;
            let next = replaced
                .entry(key)
                .or_insert_with(|| context.updated(&values, &mut self.rounded_clip_pool, &mut scene).map(std::sync::Arc::new));
            let Some(next) = next else { continue };
            *context = next.clone();
            hitbox.bounds = next.visual_bounds(*base);
            hitbox.content_mask = next.visual_mask.clone();
        }
        scene.replay(0..previous.len(), &previous);
        scene.finish();
        self.rendered_frame.scene = scene;
        self.rounded_clip_pool.snapshot(&mut self.rendered_frame, scale);
        let mut rects = std::collections::HashMap::new();
        for (id, (base, context)) in self.subtree_transform.a11y.iter_mut() {
            let key = std::sync::Arc::as_ptr(context) as usize;
            let next = match replaced.get(&key) {
                Some(next) => next.clone(),
                None => context.updated(&values, &mut self.rounded_clip_pool, &mut self.rendered_frame.scene).map(std::sync::Arc::new),
            };
            let Some(next) = next else { continue };
            *context = next.clone();
            rects.insert(*id, next.visual_bounds(*base));
        }
        if let Some(update) = self.a11y.transform_subtrees(&rects, scale) {
            self.platform_window.a11y_tree_update(update);
        }
        if changed {
            self.needs_present.set(true);
            self.on_next_frame(|window, _| window.needs_present.set(true));
        }
        true
    }
}

fn a11y_rect(bounds: Bounds<Pixels>, scale: f32) -> accesskit::Rect {
    accesskit::Rect {
        x0: (bounds.origin.x.as_f32() * scale) as f64,
        y0: (bounds.origin.y.as_f32() * scale) as f64,
        x1: ((bounds.origin.x.as_f32() + bounds.size.width.as_f32()) * scale) as f64,
        y1: ((bounds.origin.y.as_f32() + bounds.size.height.as_f32()) * scale) as f64,
    }
}

impl crate::scene::Primitive {
    /// Map an untransformed primitive (as painted) into its scope's visual position.
    pub(crate) fn transform_subtree(&mut self, context: &SubtreeTransformContext) {
        let factor = context.scale_factor;
        let m = context.matrix.scaled(factor);
        let s = m.magnitude();
        // Under a one-axis stretch, corners stay round at the smaller stretch and each border follows its own axis.
        let (sx, sy) = m.axes();
        let r = sx.min(sy);
        let opacity = context.opacity;
        // The scope's mask, narrowed by the primitive's own clip mapped through the transform (paint that narrows its
        // own mask; a border-only quad is no longer split into strips inside a scope). Only the edges narrower than the
        // context's untransformed mask (by more than GPUI's pixel snap) are the primitive's own: the others are ancestor
        // clips, which `visual_mask` already places (the one at the scope's entry stays put). Mapping those too dragged
        // an `overflow: hidden` viewport along with a panned image and cut the picture off (ADE-71).
        let mut mask = context.visual_mask.scale(factor);
        let own = self.content_mask().bounds;
        let base = context.base_mask.scale(factor);
        let narrower = [
            own.origin.x.0 > base.origin.x.0 + 1.0,
            own.origin.y.0 > base.origin.y.0 + 1.0,
            own.origin.x.0 + own.size.width.0 < base.origin.x.0 + base.size.width.0 - 1.0,
            own.origin.y.0 + own.size.height.0 < base.origin.y.0 + base.size.height.0 - 1.0,
        ];
        if narrower.iter().any(|&n| n) {
            let (ox, oy, ow, oh) = m.envelope(own.origin.x.0, own.origin.y.0, own.size.width.0, own.size.height.0);
            let b = mask.bounds;
            let (mut x0, mut y0, mut x1, mut y1) = (b.origin.x.0, b.origin.y.0, b.origin.x.0 + b.size.width.0, b.origin.y.0 + b.size.height.0);
            if narrower[0] { x0 = x0.max(ox) }
            if narrower[1] { y0 = y0.max(oy) }
            if narrower[2] { x1 = x1.min(ox + ow) }
            if narrower[3] { y1 = y1.min(oy + oh) }
            mask.bounds = Bounds::new(point(ScaledPixels(x0), ScaledPixels(y0)), size(ScaledPixels((x1 - x0).max(0.0)), ScaledPixels((y1 - y0).max(0.0))));
        }
        let rect = |b: &mut Bounds<ScaledPixels>| {
            let (x, y, w, h) = m.rect(b.origin.x.0, b.origin.y.0, b.size.width.0, b.size.height.0);
            *b = Bounds::new(point(ScaledPixels(x), ScaledPixels(y)), size(ScaledPixels(w), ScaledPixels(h)));
        };
        let corners = |c: &mut Corners<ScaledPixels>| {
            c.top_left.0 *= r;
            c.top_right.0 *= r;
            c.bottom_right.0 *= r;
            c.bottom_left.0 *= r;
        };
        let sprite = |t: &mut TransformationMatrix| {
            let r = t.rotation_scale;
            let (tx, ty) = (t.translation[0], t.translation[1]);
            t.rotation_scale = [[m.a * r[0][0] + m.b * r[1][0], m.a * r[0][1] + m.b * r[1][1]], [m.c * r[0][0] + m.d * r[1][0], m.c * r[0][1] + m.d * r[1][1]]];
            t.translation = [m.a * tx + m.b * ty + m.tx, m.c * tx + m.d * ty + m.ty];
        };
        match self {
            Self::Backdrop(p) => {
                rect(&mut p.bounds);
                p.radius.0 *= r;
                // The scope's own blur pass (painted with a unit blur and the -1 marker) follows its value.
                if p.fade_top < 0.0 {
                    p.blur = ScaledPixels(context.blur * factor * s);
                    p.fade_top = 0.0;
                    // A settled (zero) blur skips the pass entirely.
                    p.opacity = if context.blur > 0.01 { 1.0 } else { 0.0 };
                } else {
                    p.blur.0 *= s;
                    p.opacity *= opacity;
                }
                p.content_mask = mask;
            }
            Self::Shadow(p) => {
                rect(&mut p.bounds);
                rect(&mut p.element_bounds);
                corners(&mut p.corner_radii);
                corners(&mut p.element_corner_radii);
                p.blur_radius.0 *= s;
                p.color.a *= opacity * context.shadow_opacity;
                p.content_mask = mask;
            }
            Self::Quad(p) => {
                rect(&mut p.bounds);
                corners(&mut p.corner_radii);
                p.border_widths.top.0 *= sy;
                p.border_widths.right.0 *= sx;
                p.border_widths.bottom.0 *= sy;
                p.border_widths.left.0 *= sx;
                p.background = p.background.opacity(opacity);
                p.border_color.a *= opacity;
                p.content_mask = mask;
            }
            Self::Underline(p) => {
                rect(&mut p.bounds);
                p.thickness.0 *= sy;
                p.color.a *= opacity;
                p.content_mask = mask;
            }
            Self::MonochromeSprite(p) => {
                sprite(&mut p.transformation);
                p.color.a *= opacity;
                p.content_mask = mask;
            }
            Self::SubpixelSprite(p) => {
                sprite(&mut p.transformation);
                p.color.a *= opacity;
                p.content_mask = mask;
            }
            Self::PolychromeSprite(p) => {
                rect(&mut p.bounds);
                corners(&mut p.corner_radii);
                p.opacity *= opacity;
                p.content_mask = mask;
            }
            Self::Surface(p) => {
                rect(&mut p.bounds);
                p.content_mask = mask;
            }
            Self::Path(p) => {
                for vertex in &mut p.vertices {
                    let (x, y) = m.apply(vertex.xy_position.x.0, vertex.xy_position.y.0);
                    vertex.xy_position = point(ScaledPixels(x), ScaledPixels(y));
                    vertex.content_mask = mask.clone();
                }
                let (x, y, w, h) = m.envelope(p.bounds.origin.x.0, p.bounds.origin.y.0, p.bounds.size.width.0, p.bounds.size.height.0);
                p.bounds = Bounds::new(point(ScaledPixels(x), ScaledPixels(y)), size(ScaledPixels(w), ScaledPixels(h)));
                p.color = p.color.opacity(opacity);
                p.content_mask = mask;
            }
        }
    }
}
