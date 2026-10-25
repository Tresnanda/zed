// todo("windows"): remove
#![cfg_attr(windows, allow(dead_code))]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AtlasTextureId, AtlasTile, Background, Bounds, ContentMask, Corners, Edges, Hsla, Pixels,
    Point, Radians, ScaledPixels, Size, bounds_tree::BoundsTree, point,
};
use std::{
    fmt::Debug,
    iter::Peekable,
    ops::{Add, Range, Sub},
    slice,
};

#[allow(non_camel_case_types, unused)]
#[expect(missing_docs)]
pub type PathVertex_ScaledPixels = PathVertex<ScaledPixels>;

#[expect(missing_docs)]
pub type DrawOrder = u32;

/// A boolean stored as a `u32` so that GPU-facing structs contain no
/// compiler-inserted padding bytes, which would be undefined behavior to
/// reinterpret as `&[u8]` when writing instance buffers. Guaranteed to be
/// `0` or `1` by construction; shaders read it as a `u32`/`uint`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct PaddedBool32(u32);

impl From<bool> for PaddedBool32 {
    fn from(value: bool) -> Self {
        PaddedBool32(value as u32)
    }
}

#[derive(Default)]
#[expect(missing_docs)]
pub struct Scene {
    pub(crate) paint_operations: Vec<PaintOperation>,
    // A tag identifies a passive fixed-geometry leaf, never a scene position.
    // Kept outside GPU structs; shader layouts and primitive ordering are unchanged.
    /// Identity of the last ordinary scene reconstruction.
    pub paint_generation: u64,
    /// Union of passive leaf geometry touched since that reconstruction.
    pub passive_paint_bounds: Option<Bounds<ScaledPixels>>,
    /// Separate leaf regions avoid repainting the empty space between loaders.
    pub passive_paint_regions: std::collections::HashMap<u64, Bounds<ScaledPixels>>,
    repaint_slots: std::collections::HashMap<usize, (u8, usize)>,
    pub(crate) active_repaint_tag: Option<(u64, u8, f32, Option<crate::PaintAnimation>, Hsla)>,
    repaint_tags: std::collections::HashMap<usize, (u64, u8, f32, Option<crate::PaintAnimation>, Hsla)>,
    primitive_bounds: BoundsTree<ScaledPixels>,
    layer_stack: Vec<DrawOrder>,
    pub backdrops: Vec<Backdrop>,
    backdrop_order_offset: u32,
    maximum_order: u32,
    /// Rounded masks shared by all primitive types, in device pixels.
    pub rounded_clips: Vec<RoundedClip>,
    pub(crate) rounded_clip_scale: f32,
    pub shadows: Vec<Shadow>,
    pub quads: Vec<Quad>,
    pub paths: Vec<Path<ScaledPixels>>,
    pub underlines: Vec<Underline>,
    pub monochrome_sprites: Vec<MonochromeSprite>,
    pub subpixel_sprites: Vec<SubpixelSprite>,
    pub polychrome_sprites: Vec<PolychromeSprite>,
    pub surfaces: Vec<PaintSurface>,
}

#[expect(missing_docs)]
impl Scene {
    pub fn clear(&mut self) {
        self.paint_operations.clear();
        self.repaint_tags.clear();
        self.repaint_slots.clear();
        self.passive_paint_bounds = None;
        self.passive_paint_regions.clear();
        self.active_repaint_tag = None;
        self.primitive_bounds.clear();
        self.layer_stack.clear();
        self.paths.clear();
        self.backdrops.clear(); self.backdrop_order_offset=0; self.maximum_order=0;
        self.rounded_clips.clear();
        self.shadows.clear();
        self.quads.clear();
        self.underlines.clear();
        self.monochrome_sprites.clear();
        self.subpixel_sprites.clear();
        self.polychrome_sprites.clear();
        self.surfaces.clear();
    }

    pub fn len(&self) -> usize {
        self.paint_operations.len()
    }

    pub fn push_layer(&mut self, bounds: Bounds<ScaledPixels>) {
        let order = self.primitive_bounds.insert(bounds);
        self.layer_stack.push(order);
        self.paint_operations
            .push(PaintOperation::StartLayer(bounds));
    }

    pub fn pop_layer(&mut self) {
        self.layer_stack.pop();
        self.paint_operations.push(PaintOperation::EndLayer);
    }

    pub fn insert_primitive(&mut self, primitive: impl Into<Primitive>) {
        let mut primitive = primitive.into();
        let clipped_bounds = primitive
            .bounds()
            .intersect(&primitive.content_mask().bounds);

        if clipped_bounds.is_empty() {
            return;
        }

        let order = self
            .layer_stack
            .last()
            .copied()
            .unwrap_or_else(|| self.primitive_bounds.insert(clipped_bounds));
        // Filtering is an ordering barrier, including inside paint_layer.
        // Offset subsequent natural bounds-tree orders; clamping loses ordering.
        let order = if matches!(&primitive, Primitive::Backdrop(_)) { self.maximum_order + 1 } else { order + self.backdrop_order_offset };
        self.maximum_order = self.maximum_order.max(order);
        if matches!(&primitive, Primitive::Backdrop(_)) { self.backdrop_order_offset = order + 1; }
        match &mut primitive {
            Primitive::Backdrop(b) => { b.order=order; self.backdrops.push(*b); },
            Primitive::Shadow(shadow) => {
                shadow.order = order;
                self.shadows.push(*shadow);
            }
            Primitive::Quad(quad) => {
                quad.order = order;
                self.quads.push(*quad);
            }
            Primitive::Path(path) => {
                path.order = order;
                path.id = PathId(self.paths.len());
                self.paths.push(path.clone());
            }
            Primitive::Underline(underline) => {
                underline.order = order;
                self.underlines.push(*underline);
            }
            Primitive::MonochromeSprite(sprite) => {
                sprite.order = order;
                self.monochrome_sprites.push(*sprite);
            }
            Primitive::SubpixelSprite(sprite) => {
                sprite.order = order;
                self.subpixel_sprites.push(*sprite);
            }
            Primitive::PolychromeSprite(sprite) => {
                sprite.order = order;
                self.polychrome_sprites.push(*sprite);
            }
            Primitive::Surface(surface) => {
                surface.order = order;
                self.surfaces.push(surface.clone());
            }
        }
        if let Some(tag) = self.active_repaint_tag {
            // Selection washes and other decoration keep their own colors.
            if tag.1 != 2 || matches!(&primitive, Primitive::MonochromeSprite(_) | Primitive::SubpixelSprite(_)) {
                self.repaint_tags.insert(self.paint_operations.len(), tag);
            }
        }
        self.paint_operations
            .push(PaintOperation::Primitive(primitive));
    }

    pub fn replay(&mut self, range: Range<usize>, prev_scene: &Scene) {
        let previous_tag = self.active_repaint_tag;
        for index in range {
            self.active_repaint_tag = prev_scene.repaint_tags.get(&index).copied();
            let operation = &prev_scene.paint_operations[index];
            match operation {
                PaintOperation::Primitive(primitive) => self.insert_primitive(primitive.clone()),
                PaintOperation::StartLayer(bounds) => self.push_layer(*bounds),
                PaintOperation::EndLayer => self.pop_layer(),
            }
        }
        self.active_repaint_tag = previous_tag;
    }

    /// Replace only recorded quad fills / monochrome tint, preserving geometry,
    /// clipping, layer order and hit testing. All tags validate before mutation.
    pub(crate) fn repaint_tagged(&mut self, updates: &[(u64, Hsla)]) -> bool {
        self.repaint_tagged_inner(updates, true)
    }
    fn repaint_tagged_inner(&mut self, updates: &[(u64, Hsla)], update_base: bool) -> bool {
        let colors: std::collections::HashMap<_, _> = updates.iter().copied().collect();
        let mut found = std::collections::HashSet::new();
        for (&index, &(id, kind, _, _, _)) in &self.repaint_tags {
            if !colors.contains_key(&id) { continue; }
            let valid = matches!((&self.paint_operations[index], kind),
                (PaintOperation::Primitive(Primitive::Quad(_)), 0) |
                (PaintOperation::Primitive(Primitive::MonochromeSprite(_)), 1 | 2) |
                (PaintOperation::Primitive(Primitive::SubpixelSprite(_)), 2));
            let Some(&(slot_kind, slot)) = self.repaint_slots.get(&index) else { return false; };
            if !valid || (slot_kind != kind && !(kind == 2 && slot_kind == 3)) || match slot_kind {
                0 => slot >= self.quads.len(),
                1 | 2 => slot >= self.monochrome_sprites.len(),
                3 => slot >= self.subpixel_sprites.len(),
                _ => true,
            } { return false; }
            found.insert(id);
        }
        if found.len() != colors.len() { return false; }
        let pop_centres = self.pop_centres();
        for (&index, &(id, kind, inherited_opacity, animation, _)) in &self.repaint_tags {
            let Some(color) = colors.get(&id) else { continue; };
            let color = color.opacity(inherited_opacity);
            let bounds = match &self.paint_operations[index] {
                PaintOperation::Primitive(primitive) => primitive.bounds().intersect(&primitive.content_mask().bounds),
                _ => unreachable!("validated paint primitive"),
            };
            self.passive_paint_regions.entry(id).and_modify(|old| *old = old.union(&bounds)).or_insert(bounds);
            self.passive_paint_bounds = Some(self.passive_paint_bounds.map_or(bounds, |old| old.union(&bounds)));
            match (&mut self.paint_operations[index], kind) {
                (PaintOperation::Primitive(Primitive::Quad(quad)), 0) => {
                    quad.background = color.into();
                    self.quads[self.repaint_slots[&index].1].background = quad.background;
                },
                (PaintOperation::Primitive(Primitive::MonochromeSprite(sprite)), 1 | 2) => {
                    sprite.color = color;
                    self.monochrome_sprites[self.repaint_slots[&index].1].color = color;
                    if let Some(animation) = animation.filter(|animation| animation.pop) {
                        if let Some(&(cx, cy)) = pop_centres.get(&id) {
                            let scale = animation.pop_state(paint_animation_clock(animation)).1;
                            let grow = TransformationMatrix::unit().translate(Point::new(ScaledPixels(cx), ScaledPixels(cy))).scale(crate::Size { width: scale, height: scale }).translate(Point::new(ScaledPixels(-cx), ScaledPixels(-cy)));
                            sprite.transformation = grow;
                            self.monochrome_sprites[self.repaint_slots[&index].1].transformation = grow;
                        }
                    }
                    if let Some(animation) = animation.filter(|animation| animation.rotate) {
                        let centre = Point::new(ScaledPixels(sprite.bounds.origin.x.0 + sprite.bounds.size.width.0 / 2.0), ScaledPixels(sprite.bounds.origin.y.0 + sprite.bounds.size.height.0 / 2.0));
                        let turn = TransformationMatrix::unit().translate(centre).rotate(Radians(animation.angle(paint_animation_clock(animation)))).translate(Point::new(ScaledPixels(-centre.x.0), ScaledPixels(-centre.y.0)));
                        sprite.transformation = turn;
                        self.monochrome_sprites[self.repaint_slots[&index].1].transformation = turn;
                    }
                },
                (PaintOperation::Primitive(Primitive::SubpixelSprite(sprite)), 2) => {
                    sprite.color=color;
                    self.subpixel_sprites[self.repaint_slots[&index].1].color=color;
                    if let Some(animation) = animation.filter(|animation| animation.pop) {
                        if let Some(&(cx, cy)) = pop_centres.get(&id) {
                            let scale = animation.pop_state(paint_animation_clock(animation)).1;
                            let grow = TransformationMatrix::unit().translate(Point::new(ScaledPixels(cx), ScaledPixels(cy))).scale(crate::Size { width: scale, height: scale }).translate(Point::new(ScaledPixels(-cx), ScaledPixels(-cy)));
                            sprite.transformation = grow;
                            self.subpixel_sprites[self.repaint_slots[&index].1].transformation = grow;
                        }
                    }

                },
                _ => unreachable!("validated above"),
            }
        }
        if update_base {
            for (id, _, _, _, color) in self.repaint_tags.values_mut() {
                if let Some(next) = colors.get(id) { *color = *next; }
            }
        }
        // Geometry and sort keys did not change. Preserve the scene's existing
        // buffers, bounds tree, atlas tiles, cached ranges and backdrop barriers.
        // Keep the previous algorithm selectable for same-binary measurements.
        static DIRECT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*DIRECT.get_or_init(|| std::env::var("KYBERN_SCOPED_BUFFERS").as_deref() != Ok("0")) {
            let previous = std::mem::take(self);
            self.replay(0..previous.len(), &previous);
        self.rounded_clips = previous.rounded_clips.clone();
        self.rounded_clip_scale = previous.rounded_clip_scale;
            self.finish();
        }
        true
    }

    /// Whether this scene contains a visible declarative passive-paint loop.
    pub fn has_native_paint_animation(&self) -> bool {
        self.repaint_tags.values().any(|(_,_,_,animation,_)| animation.is_some())
    }

    pub fn finish(&mut self) {
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        self.paint_generation = GENERATION.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
        // Stable sorts below retain insertion order among equal keys. Derive
        // slots from the same operation order, including untagged primitives.
        // This work happens only after an ordinary scene rebuild, never per tint.
        self.repaint_slots.clear();
        self.passive_paint_bounds = None;
        self.passive_paint_regions.clear();
        if !self.repaint_tags.is_empty() {
            let mut quads = Vec::new();
            let mut sprites = Vec::new();
            let mut subpixel = Vec::new();
            for (index, operation) in self.paint_operations.iter().enumerate() {
                match operation {
                    PaintOperation::Primitive(Primitive::Quad(q)) => quads.push((index, q.order)),
                    PaintOperation::Primitive(Primitive::MonochromeSprite(p)) => sprites.push((index, (p.order, p.tile.tile_id))),
                    PaintOperation::Primitive(Primitive::SubpixelSprite(p)) => subpixel.push((index,(p.order,p.tile.tile_id))),
                    _ => {},
                }
            }
            quads.sort_by_key(|(_, order)| *order);
            sprites.sort_by_key(|(_, order)| *order);
            subpixel.sort_by_key(|(_, order)| *order);
            for (slot, (index, _)) in quads.into_iter().enumerate() {
                if self.repaint_tags.contains_key(&index) { self.repaint_slots.insert(index, (0, slot)); }
            }
            for (slot, (index, _)) in sprites.into_iter().enumerate() {
                if self.repaint_tags.contains_key(&index) { self.repaint_slots.insert(index, (self.repaint_tags[&index].1, slot)); }
            }
            for (slot,(index,_)) in subpixel.into_iter().enumerate() {
                if self.repaint_tags.contains_key(&index) { self.repaint_slots.insert(index,(3,slot)); }
            }
        }
        self.backdrops.sort_by_key(|b| b.order);
        self.shadows.sort_by_key(|shadow| shadow.order);
        self.quads.sort_by_key(|quad| quad.order);
        self.paths.sort_by_key(|path| path.order);
        self.underlines.sort_by_key(|underline| underline.order);
        self.monochrome_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.tile_id));
        self.subpixel_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.tile_id));
        self.polychrome_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.tile_id));
        self.surfaces.sort_by_key(|surface| surface.order);
    }

    #[cfg_attr(
        all(
            any(target_os = "linux", target_os = "freebsd"),
            not(any(feature = "x11", feature = "wayland"))
        ),
        allow(dead_code)
    )]
    pub fn batches(&self) -> impl Iterator<Item = PrimitiveBatch> + '_ {
        BatchIterator {
            backdrops_start: 0, backdrops_iter: self.backdrops.iter().peekable(),
            shadows_start: 0,
            shadows_iter: self.shadows.iter().peekable(),
            quads_start: 0,
            quads_iter: self.quads.iter().peekable(),
            paths_start: 0,
            paths_iter: self.paths.iter().peekable(),
            underlines_start: 0,
            underlines_iter: self.underlines.iter().peekable(),
            monochrome_sprites_start: 0,
            monochrome_sprites_iter: self.monochrome_sprites.iter().peekable(),
            subpixel_sprites_start: 0,
            subpixel_sprites_iter: self.subpixel_sprites.iter().peekable(),
            polychrome_sprites_start: 0,
            polychrome_sprites_iter: self.polychrome_sprites.iter().peekable(),
            surfaces_start: 0,
            surfaces_iter: self.surfaces.iter().peekable(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Default)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
pub(crate) enum PrimitiveKind {
    Backdrop,
    Shadow,
    #[default]
    Quad,
    Path,
    Underline,
    MonochromeSprite,
    SubpixelSprite,
    PolychromeSprite,
    Surface,
}

pub(crate) enum PaintOperation {
    Primitive(Primitive),
    StartLayer(Bounds<ScaledPixels>),
    EndLayer,
}

#[derive(Clone)]
#[expect(missing_docs)]
pub enum Primitive {
    Backdrop(Backdrop),
    Shadow(Shadow),
    Quad(Quad),
    Path(Path<ScaledPixels>),
    Underline(Underline),
    MonochromeSprite(MonochromeSprite),
    SubpixelSprite(SubpixelSprite),
    PolychromeSprite(PolychromeSprite),
    Surface(PaintSurface),
}

#[expect(missing_docs)]
impl Primitive {
    pub fn bounds(&self) -> &Bounds<ScaledPixels> {
        match self {
            Primitive::Backdrop(b) => &b.bounds,
            Primitive::Shadow(shadow) => &shadow.bounds,
            Primitive::Quad(quad) => &quad.bounds,
            Primitive::Path(path) => &path.bounds,
            Primitive::Underline(underline) => &underline.bounds,
            Primitive::MonochromeSprite(sprite) => &sprite.bounds,
            Primitive::SubpixelSprite(sprite) => &sprite.bounds,
            Primitive::PolychromeSprite(sprite) => &sprite.bounds,
            Primitive::Surface(surface) => &surface.bounds,
        }
    }

    pub fn content_mask(&self) -> &ContentMask<ScaledPixels> {
        match self {
            Primitive::Backdrop(b) => &b.content_mask,
            Primitive::Shadow(shadow) => &shadow.content_mask,
            Primitive::Quad(quad) => &quad.content_mask,
            Primitive::Path(path) => &path.content_mask,
            Primitive::Underline(underline) => &underline.content_mask,
            Primitive::MonochromeSprite(sprite) => &sprite.content_mask,
            Primitive::SubpixelSprite(sprite) => &sprite.content_mask,
            Primitive::PolychromeSprite(sprite) => &sprite.content_mask,
            Primitive::Surface(surface) => &surface.content_mask,
        }
    }
}

#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
struct BatchIterator<'a> {
    backdrops_start: usize, backdrops_iter: Peekable<slice::Iter<'a, Backdrop>>,
    shadows_start: usize,
    shadows_iter: Peekable<slice::Iter<'a, Shadow>>,
    quads_start: usize,
    quads_iter: Peekable<slice::Iter<'a, Quad>>,
    paths_start: usize,
    paths_iter: Peekable<slice::Iter<'a, Path<ScaledPixels>>>,
    underlines_start: usize,
    underlines_iter: Peekable<slice::Iter<'a, Underline>>,
    monochrome_sprites_start: usize,
    monochrome_sprites_iter: Peekable<slice::Iter<'a, MonochromeSprite>>,
    subpixel_sprites_start: usize,
    subpixel_sprites_iter: Peekable<slice::Iter<'a, SubpixelSprite>>,
    polychrome_sprites_start: usize,
    polychrome_sprites_iter: Peekable<slice::Iter<'a, PolychromeSprite>>,
    surfaces_start: usize,
    surfaces_iter: Peekable<slice::Iter<'a, PaintSurface>>,
}

impl<'a> Iterator for BatchIterator<'a> {
    type Item = PrimitiveBatch;

    fn next(&mut self) -> Option<Self::Item> {
        let mut orders_and_kinds = [
            (self.backdrops_iter.peek().map(|b|b.order),PrimitiveKind::Backdrop),
            (
                self.shadows_iter.peek().map(|s| s.order),
                PrimitiveKind::Shadow,
            ),
            (self.quads_iter.peek().map(|q| q.order), PrimitiveKind::Quad),
            (self.paths_iter.peek().map(|q| q.order), PrimitiveKind::Path),
            (
                self.underlines_iter.peek().map(|u| u.order),
                PrimitiveKind::Underline,
            ),
            (
                self.monochrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::MonochromeSprite,
            ),
            (
                self.subpixel_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::SubpixelSprite,
            ),
            (
                self.polychrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::PolychromeSprite,
            ),
            (
                self.surfaces_iter.peek().map(|s| s.order),
                PrimitiveKind::Surface,
            ),
        ];
        orders_and_kinds.sort_by_key(|(order, kind)| (order.unwrap_or(u32::MAX), *kind));

        let first = orders_and_kinds[0];
        let second = orders_and_kinds[1];
        let (batch_kind, max_order_and_kind) = if first.0.is_some() {
            (first.1, (second.0.unwrap_or(u32::MAX), second.1))
        } else {
            return None;
        };

        match batch_kind {
            PrimitiveKind::Backdrop => { let i=self.backdrops_start; self.backdrops_start+=1; self.backdrops_iter.next(); Some(PrimitiveBatch::Backdrop(i)) },
            PrimitiveKind::Shadow => {
                let shadows_start = self.shadows_start;
                let mut shadows_end = shadows_start + 1;
                self.shadows_iter.next();
                while self
                    .shadows_iter
                    .next_if(|shadow| (shadow.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    shadows_end += 1;
                }
                self.shadows_start = shadows_end;
                Some(PrimitiveBatch::Shadows(shadows_start..shadows_end))
            }
            PrimitiveKind::Quad => {
                let quads_start = self.quads_start;
                let mut quads_end = quads_start + 1;
                self.quads_iter.next();
                while self
                    .quads_iter
                    .next_if(|quad| (quad.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    quads_end += 1;
                }
                self.quads_start = quads_end;
                Some(PrimitiveBatch::Quads(quads_start..quads_end))
            }
            PrimitiveKind::Path => {
                let paths_start = self.paths_start;
                let mut paths_end = paths_start + 1;
                self.paths_iter.next();
                while self
                    .paths_iter
                    .next_if(|path| (path.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    paths_end += 1;
                }
                self.paths_start = paths_end;
                Some(PrimitiveBatch::Paths(paths_start..paths_end))
            }
            PrimitiveKind::Underline => {
                let underlines_start = self.underlines_start;
                let mut underlines_end = underlines_start + 1;
                self.underlines_iter.next();
                while self
                    .underlines_iter
                    .next_if(|underline| (underline.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    underlines_end += 1;
                }
                self.underlines_start = underlines_end;
                Some(PrimitiveBatch::Underlines(underlines_start..underlines_end))
            }
            PrimitiveKind::MonochromeSprite => {
                let texture_id = self.monochrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.monochrome_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.monochrome_sprites_iter.next();
                while self
                    .monochrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.monochrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::SubpixelSprite => {
                let texture_id = self.subpixel_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.subpixel_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.subpixel_sprites_iter.next();
                while self
                    .subpixel_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.subpixel_sprites_start = sprites_end;
                Some(PrimitiveBatch::SubpixelSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::PolychromeSprite => {
                let texture_id = self.polychrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.polychrome_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.polychrome_sprites_iter.next();
                while self
                    .polychrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.polychrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::Surface => {
                let surfaces_start = self.surfaces_start;
                let mut surfaces_end = surfaces_start + 1;
                self.surfaces_iter.next();
                while self
                    .surfaces_iter
                    .next_if(|surface| (surface.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    surfaces_end += 1;
                }
                self.surfaces_start = surfaces_end;
                Some(PrimitiveBatch::Surfaces(surfaces_start..surfaces_end))
            }
        }
    }
}

#[derive(Debug)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
#[allow(missing_docs)]
pub enum PrimitiveBatch {
    Backdrop(usize),
    Shadows(Range<usize>),
    Quads(Range<usize>),
    Paths(Range<usize>),
    Underlines(Range<usize>),
    MonochromeSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    SubpixelSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    PolychromeSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    Surfaces(Range<usize>),
}

impl PrimitiveBatch {
    #[expect(missing_docs)]
    pub fn label(&self) -> String {
        match self {
            Self::Backdrop(_) => "backdrop".into(),
            Self::Shadows(range) => format!("shadows ({})", range.len()),
            Self::Quads(range) => format!("quads ({})", range.len()),
            Self::Paths(range) => format!("paths ({})", range.len()),
            Self::Underlines(range) => format!("underlines ({})", range.len()),
            Self::MonochromeSprites { texture_id, range } => {
                format!(
                    "monochrome sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::SubpixelSprites { texture_id, range } => {
                format!(
                    "subpixel sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::PolychromeSprites { texture_id, range } => {
                format!(
                    "polychrome sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::Surfaces(range) => format!("surfaces ({})", range.len()),
        }
    }
}

#[derive(Default, Debug, Copy, Clone)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Quad {
    pub order: DrawOrder,
    pub border_style: BorderStyle,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub background: Background,
    pub border_color: Hsla,
    pub corner_radii: Corners<ScaledPixels>,
    pub border_widths: Edges<ScaledPixels>,
}

impl From<Quad> for Primitive {
    fn from(quad: Quad) -> Self {
        Primitive::Quad(quad)
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Underline {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub thickness: ScaledPixels,
    pub wavy: PaddedBool32,
}

impl From<Underline> for Primitive {
    fn from(underline: Underline) -> Self {
        Primitive::Underline(underline)
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Shadow {
    pub order: DrawOrder,
    pub blur_radius: ScaledPixels,
    pub bounds: Bounds<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub element_bounds: Bounds<ScaledPixels>,
    pub element_corner_radii: Corners<ScaledPixels>,
    /// 0 = drop shadow (rendered outside the element), 1 = inset shadow (rendered inside).
    pub inset: u32,
    pub pad: u32, // align to 8 bytes
}

impl From<Shadow> for Primitive {
    fn from(shadow: Shadow) -> Self {
        Primitive::Shadow(shadow)
    }
}

/// The style of a border.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[repr(C)]
pub enum BorderStyle {
    /// A solid border.
    #[default]
    Solid = 0,
    /// A dashed border.
    Dashed = 1,
}

/// A data type representing a 2 dimensional transformation that can be applied to an element.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct TransformationMatrix {
    /// 2x2 matrix containing rotation and scale,
    /// stored row-major
    pub rotation_scale: [[f32; 2]; 2],
    /// translation vector
    pub translation: [f32; 2],
}

impl Eq for TransformationMatrix {}

impl TransformationMatrix {
    /// The unit matrix, has no effect.
    pub fn unit() -> Self {
        Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [0.0, 0.0],
        }
    }

    /// Move the origin by a given point
    pub fn translate(mut self, point: Point<ScaledPixels>) -> Self {
        self.compose(Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [point.x.0, point.y.0],
        })
    }

    /// Clockwise rotation in radians around the origin
    pub fn rotate(self, angle: Radians) -> Self {
        self.compose(Self {
            rotation_scale: [
                [angle.0.cos(), -angle.0.sin()],
                [angle.0.sin(), angle.0.cos()],
            ],
            translation: [0.0, 0.0],
        })
    }

    /// Scale around the origin
    pub fn scale(self, size: Size<f32>) -> Self {
        self.compose(Self {
            rotation_scale: [[size.width, 0.0], [0.0, size.height]],
            translation: [0.0, 0.0],
        })
    }

    /// Perform matrix multiplication with another transformation
    /// to produce a new transformation that is the result of
    /// applying both transformations: first, `other`, then `self`.
    #[inline]
    pub fn compose(self, other: TransformationMatrix) -> TransformationMatrix {
        if other == Self::unit() {
            return self;
        }
        // Perform matrix multiplication
        TransformationMatrix {
            rotation_scale: [
                [
                    self.rotation_scale[0][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][0],
                    self.rotation_scale[0][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][1],
                ],
                [
                    self.rotation_scale[1][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][0],
                    self.rotation_scale[1][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][1],
                ],
            ],
            translation: [
                self.translation[0]
                    + self.rotation_scale[0][0] * other.translation[0]
                    + self.rotation_scale[0][1] * other.translation[1],
                self.translation[1]
                    + self.rotation_scale[1][0] * other.translation[0]
                    + self.rotation_scale[1][1] * other.translation[1],
            ],
        }
    }

    /// Apply transformation to a point, mainly useful for debugging
    pub fn apply(&self, point: Point<Pixels>) -> Point<Pixels> {
        let input = [point.x.0, point.y.0];
        let mut output = self.translation;
        for (i, output_cell) in output.iter_mut().enumerate() {
            for (k, input_cell) in input.iter().enumerate() {
                *output_cell += self.rotation_scale[i][k] * *input_cell;
            }
        }
        Point::new(output[0].into(), output[1].into())
    }
}

impl Default for TransformationMatrix {
    fn default() -> Self {
        Self::unit()
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct MonochromeSprite {
    pub order: DrawOrder,
    pub pad: u32,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub tile: AtlasTile,
    pub transformation: TransformationMatrix,
}

impl From<MonochromeSprite> for Primitive {
    fn from(sprite: MonochromeSprite) -> Self {
        Primitive::MonochromeSprite(sprite)
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct SubpixelSprite {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub tile: AtlasTile,
    pub transformation: TransformationMatrix,
}

impl From<SubpixelSprite> for Primitive {
    fn from(sprite: SubpixelSprite) -> Self {
        Primitive::SubpixelSprite(sprite)
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct PolychromeSprite {
    pub order: DrawOrder,
    pub nearest_neighbor: PaddedBool32,
    pub grayscale: PaddedBool32,
    pub opacity: f32,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
    pub tile: AtlasTile,
}

impl From<PolychromeSprite> for Primitive {
    fn from(sprite: PolychromeSprite) -> Self {
        Primitive::PolychromeSprite(sprite)
    }
}

#[derive(Clone)]
#[allow(missing_docs)]
pub struct PaintSurface {
    pub order: DrawOrder,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    #[cfg(target_os = "macos")]
    pub image_buffer: core_video::pixel_buffer::CVPixelBuffer,
    /// Type-erased GPU texture (`Arc<wgpu::Texture>`). Ported from gpui-ce
    /// ([#39](https://github.com/gpui-ce/gpui-ce/commit/6d043b22e477)).
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub texture: std::sync::Arc<dyn std::any::Any + Send + Sync>,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub texture_size: Size<crate::DevicePixels>,
}

impl std::fmt::Debug for PaintSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("PaintSurface");
        debug
            .field("order", &self.order)
            .field("bounds", &self.bounds)
            .field("content_mask", &self.content_mask);
        #[cfg(target_os = "macos")]
        debug.field("image_buffer", &self.image_buffer);
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        debug.field("texture_size", &self.texture_size);
        debug.finish_non_exhaustive()
    }
}

impl From<PaintSurface> for Primitive {
    fn from(surface: PaintSurface) -> Self {
        Primitive::Surface(surface)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[expect(missing_docs)]
pub struct PathId(pub usize);

/// A line made up of a series of vertices and control points.
#[derive(Clone, Debug)]
#[expect(missing_docs)]
pub struct Path<P: Clone + Debug + Default + PartialEq> {
    pub id: PathId,
    pub order: DrawOrder,
    pub bounds: Bounds<P>,
    pub content_mask: ContentMask<P>,
    pub vertices: Vec<PathVertex<P>>,
    pub color: Background,
    start: Point<P>,
    current: Point<P>,
    contour_count: usize,
}

impl Path<Pixels> {
    /// Create a new path with the given starting point.
    pub fn new(start: Point<Pixels>) -> Self {
        Self {
            id: PathId(0),
            order: DrawOrder::default(),
            vertices: Vec::new(),
            start,
            current: start,
            bounds: Bounds {
                origin: start,
                size: Default::default(),
            },
            content_mask: Default::default(),
            color: Default::default(),
            contour_count: 0,
        }
    }

    /// Scale this path by the given factor.
    pub fn scale(&self, factor: f32) -> Path<ScaledPixels> {
        Path {
            id: self.id,
            order: self.order,
            bounds: self.bounds.scale(factor),
            content_mask: self.content_mask.scale(factor),
            vertices: self
                .vertices
                .iter()
                .map(|vertex| vertex.scale(factor))
                .collect(),
            start: self.start.map(|start| start.scale(factor)),
            current: self.current.scale(factor),
            contour_count: self.contour_count,
            color: self.color,
        }
    }

    /// Move the start, current point to the given point.
    pub fn move_to(&mut self, to: Point<Pixels>) {
        self.contour_count += 1;
        self.start = to;
        self.current = to;
    }

    /// Draw a straight line from the current point to the given point.
    pub fn line_to(&mut self, to: Point<Pixels>) {
        self.contour_count += 1;
        if self.contour_count > 1 {
            self.push_triangle(
                (self.start, self.current, to),
                (point(0., 1.), point(0., 1.), point(0., 1.)),
            );
        }
        self.current = to;
    }

    /// Draw a curve from the current point to the given point, using the given control point.
    pub fn curve_to(&mut self, to: Point<Pixels>, ctrl: Point<Pixels>) {
        self.contour_count += 1;
        if self.contour_count > 1 {
            self.push_triangle(
                (self.start, self.current, to),
                (point(0., 1.), point(0., 1.), point(0., 1.)),
            );
        }

        self.push_triangle(
            (self.current, ctrl, to),
            (point(0., 0.), point(0.5, 0.), point(1., 1.)),
        );
        self.current = to;
    }

    /// Push a triangle to the Path.
    pub fn push_triangle(
        &mut self,
        xy: (Point<Pixels>, Point<Pixels>, Point<Pixels>),
        st: (Point<f32>, Point<f32>, Point<f32>),
    ) {
        self.bounds = self
            .bounds
            .union(&Bounds {
                origin: xy.0,
                size: Default::default(),
            })
            .union(&Bounds {
                origin: xy.1,
                size: Default::default(),
            })
            .union(&Bounds {
                origin: xy.2,
                size: Default::default(),
            });

        self.vertices.push(PathVertex {
            xy_position: xy.0,
            st_position: st.0,
            content_mask: Default::default(),
        });
        self.vertices.push(PathVertex {
            xy_position: xy.1,
            st_position: st.1,
            content_mask: Default::default(),
        });
        self.vertices.push(PathVertex {
            xy_position: xy.2,
            st_position: st.2,
            content_mask: Default::default(),
        });
    }
}

impl<T> Path<T>
where
    T: Clone + Debug + Default + PartialEq + PartialOrd + Add<T, Output = T> + Sub<Output = T>,
{
    #[allow(unused)]
    #[expect(missing_docs)]
    pub fn clipped_bounds(&self) -> Bounds<T> {
        self.bounds.intersect(&self.content_mask.bounds)
    }
}

impl From<Path<ScaledPixels>> for Primitive {
    fn from(path: Path<ScaledPixels>) -> Self {
        Primitive::Path(path)
    }
}

#[derive(Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct PathVertex<P: Clone + Debug + Default + PartialEq> {
    pub xy_position: Point<P>,
    pub st_position: Point<f32>,
    pub content_mask: ContentMask<P>,
}

#[expect(missing_docs)]
impl PathVertex<Pixels> {
    pub fn scale(&self, factor: f32) -> PathVertex<ScaledPixels> {
        PathVertex {
            xy_position: self.xy_position.scale(factor),
            st_position: self.st_position,
            content_mask: self.content_mask.scale(factor),
        }
    }
}

/// A scene-local backdrop filter. Only the Metal backend implements it.
#[derive(Clone,Copy,Debug)]
#[allow(missing_docs)]
pub struct Backdrop {
    pub order: DrawOrder,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub blur: ScaledPixels,
    pub radius: ScaledPixels,
    pub saturation: f32,
    pub opacity: f32,
    pub fade_top: f32,
}
impl From<Backdrop> for Primitive { fn from(b:Backdrop)->Self {Self::Backdrop(b)} }

/// Declarative paint-only loop, evaluated at native display callbacks.
#[derive(Clone, Copy, Debug)]
#[allow(missing_docs)]
pub struct PaintAnimation {
    pub cycle_ms: f64,
    pub delay_ms: f64,
    pub breathe: bool,
    pub shimmer: bool,
    pub rotate: bool,
    pub pop: bool,
    pub started: std::time::Instant,
}
impl PaintAnimation {
    pub(crate) fn same_config(self, other: Self) -> bool {
        self.cycle_ms == other.cycle_ms && self.delay_ms == other.delay_ms && self.breathe == other.breathe && self.shimmer == other.shimmer && self.rotate == other.rotate && self.pop == other.pop
    }
    fn opacity(self, now: std::time::Instant) -> f32 {
        if self.rotate { return 1.0; }
        if self.pop { return self.pop_state(now).0; }
        let elapsed = now.saturating_duration_since(self.started).as_secs_f64() * 1000.0 - self.delay_ms;
        if elapsed < 0.0 { return if self.breathe { 1.0 } else { 0.22 }; }
        let phase = (elapsed % self.cycle_ms) / self.cycle_ms;
        fn ease(x: f64) -> f64 {
            let bez = |t: f64, a: f64, b: f64| 3.0*a*t*(1.0-t).powi(2)+3.0*b*t*t*(1.0-t)+t*t*t;
            let (mut lo, mut hi) = (0.0, 1.0);
            for _ in 0..16 { let mid=(lo+hi)*0.5; if bez(mid,0.42,0.58)<x {lo=mid;} else {hi=mid;} }
            bez((lo+hi)*0.5,0.0,1.0)
        }
        (if self.shimmer { 1.0 + 0.6*ease(if phase<0.5 {phase*2.0} else {2.0-phase*2.0}) }
        else if self.breathe { 1.0 - 0.55*ease(if phase<0.5 {phase*2.0} else {2.0-phase*2.0}) }
        else if phase<0.15 { 0.22 + 0.78*ease(phase/0.15) }
        else if phase<0.45 { 1.0 - 0.78*ease((phase-0.15)/0.3) }
        else { 0.22 }) as f32
    }
}
impl PaintAnimation {
    /// Clockwise radians of a rotating glyph: one linear turn per `cycle_ms`.
    pub(crate) fn angle(self, at: std::time::Instant) -> f32 {
        let elapsed = at.saturating_duration_since(self.started).as_secs_f64() * 1000.0;
        ((elapsed % self.cycle_ms) / self.cycle_ms * std::f64::consts::TAU) as f32
    }
}
/// The animation clock, frozen by `KYBERN_PAINT_PHASE_MS` for pixel comparisons (as `animate_tagged` does).
impl PaintAnimation {
    /// `t-pop` at `at`: (opacity, scale). `both` fill: the first keyframe during the delay, the last after the end.
    pub(crate) fn pop_state(self, at: std::time::Instant) -> (f32, f32) {
        let elapsed = at.saturating_duration_since(self.started).as_secs_f64() * 1000.0 - self.delay_ms;
        let x = (elapsed / self.cycle_ms).clamp(0.0, 1.0);
        // `--ease-bounce`: y(t) of cubic-bezier(0.34, 1.36, 0.64, 1) at the t whose x(t) is `x`.
        let (x1, y1, x2, y2) = (0.34f64, 1.36f64, 0.64f64, 1.0f64);
        let bez = |t: f64, a: f64, b: f64| 3.0 * a * t * (1.0 - t).powi(2) + 3.0 * b * t * t * (1.0 - t) + t * t * t;
        let (mut lo, mut hi) = (0.0f64, 1.0f64);
        for _ in 0..24 { let mid = (lo + hi) * 0.5; if bez(mid, x1, x2) < x { lo = mid; } else { hi = mid; } }
        let e = bez((lo + hi) * 0.5, y1, y2);
        ((0.4 + 0.6 * e).clamp(0.0, 1.0) as f32, (0.85 + 0.15 * e) as f32)
    }
    /// The curve has ended and the last frames (100 ms of margin) have been painted.
    pub(crate) fn pop_done(self, now: std::time::Instant) -> bool {
        now.saturating_duration_since(self.started).as_secs_f64() * 1000.0 >= self.delay_ms + self.cycle_ms + 100.0
    }
}
fn paint_animation_clock(animation: PaintAnimation) -> std::time::Instant {
    static PHASE: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    let phase = *PHASE.get_or_init(|| std::env::var("KYBERN_PAINT_PHASE_MS").ok().and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v >= 0.0));
    phase.map_or_else(std::time::Instant::now, |ms| animation.started + std::time::Duration::from_secs_f64(ms / 1000.0))
}
impl Scene {
    fn pop_centres(&self) -> std::collections::HashMap<u64, (f32, f32)> {
        let mut boxes: std::collections::HashMap<u64, [f32; 4]> = std::collections::HashMap::new();
        for (&index, &(id, _, _, animation, _)) in &self.repaint_tags {
            if !animation.is_some_and(|a| a.pop) { continue; }
            let b = match &self.paint_operations[index] {
                PaintOperation::Primitive(Primitive::MonochromeSprite(p)) => p.bounds,
                PaintOperation::Primitive(Primitive::SubpixelSprite(p)) => p.bounds,
                _ => continue,
            };
            let (x0, y0, x1, y1) = (b.origin.x.0, b.origin.y.0, b.origin.x.0 + b.size.width.0, b.origin.y.0 + b.size.height.0);
            let e = boxes.entry(id).or_insert([x0, y0, x1, y1]);
            e[0] = e[0].min(x0); e[1] = e[1].min(y0); e[2] = e[2].max(x1); e[3] = e[3].max(y1);
        }
        boxes.into_iter().map(|(id, b)| (id, ((b[0] + b[2]) * 0.5, (b[1] + b[3]) * 0.5))).collect()
    }
    pub(crate) fn animation_for(&self, id: u64) -> Option<PaintAnimation> {
        self.repaint_tags.values().find_map(|&(tag, _, _, animation, _)| if tag == id {animation} else {None})
    }
    pub(crate) fn animate_tagged(&mut self) -> bool {
        let now = std::time::Instant::now();
        // Text contributes a primitive per glyph, but its phase/base tint are
        // shared by the tagged leaf. Evaluate the curve only once per leaf.
        let mut seen = std::collections::HashSet::new();
        let updates: Vec<_> = self.repaint_tags.values().filter_map(|&(id, _, _, animation, color)| {
            if animation.is_none() || animation.is_some_and(|a| a.pop && a.pop_done(paint_animation_clock(a))) || !seen.insert(id) { return None; }
            animation.map(|animation| {
                // Freeze only when explicitly requested by pixel comparisons.
                static PHASE: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
                let phase = *PHASE.get_or_init(|| std::env::var("KYBERN_PAINT_PHASE_MS").ok().and_then(|v|v.parse::<f64>().ok()).filter(|v|v.is_finite() && *v>=0.0));
                let at = phase.map_or(now, |ms|animation.started + std::time::Duration::from_secs_f64(ms/1000.0));
                let mut next = color;
                next.a = (color.a * animation.opacity(at)).clamp(0.0,1.0);
                (id,next)
            })
        }).collect();
        if updates.is_empty() { return false; }
        let ok = self.repaint_tagged_inner(&updates, false);
        if !ok && std::env::var_os("KYBERN_DEBUG_PAINT_ANIMATION").is_some() {
            let present: std::collections::HashSet<u64> = self.repaint_tags.values().map(|t| t.0).collect();
            eprintln!("[paint-animation] stopped: {} tagged leaves, ids without paint: {:?}", updates.len(), updates.iter().filter(|(id, _)| !present.contains(id)).map(|(id, _)| *id).collect::<Vec<_>>());
        }
        ok
    }
}

/// One immutable link of an arbitrarily nested rounded content mask. `kind` 1
/// is an edge fade (N3): `radii` then hold the top, right, bottom and left ramps.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
#[allow(missing_docs)]
pub struct RoundedClip { pub bounds: [f32;4], pub radii: [f32;4], pub parent: u32, pub kind: u32 }
impl Scene {
    pub(crate) fn rounded_contains(&self, mut id:u32, point:Point<Pixels>) -> bool {
        let x=point.x.as_f32()*self.rounded_clip_scale; let y=point.y.as_f32()*self.rounded_clip_scale;
        while id!=0 {
            let Some(n)=self.rounded_clips.get(id as usize) else {return false;};
            // Hit testing ignores edge fades.
            if n.kind!=0 { id=n.parent; continue; }
            let [left,top,width,height]=n.bounds;
            if x<left || y<top || x>=left+width || y>=top+height { return false; }
            let right=x>=left+width/2.; let bottom=y>=top+height/2.;
            let radius=n.radii[match(right,bottom){(false,false)=>0,(true,false)=>1,(true,true)=>2,(false,true)=>3}];
            let cx=if right {left+width-radius} else {left+radius};
            let cy=if bottom {top+height-radius} else {top+radius};
            let dx=if right {(x-cx).max(0.)} else {(cx-x).max(0.)};
            let dy=if bottom {(y-cy).max(0.)} else {(cy-y).max(0.)};
            if dx*dx+dy*dy>radius*radius { return false; }
            id=n.parent;
        }
        true
    }
}
