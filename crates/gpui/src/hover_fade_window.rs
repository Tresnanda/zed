// Included in gpui::window (hover_fade_gpui.py). Paint-only hover/active colour transitions: an interactive element with
// a declared `ColorTransition` eases its background and border colours between rest, hover and release instead of
// snapping (the desktop's `transition-colors`). The flip itself renders once, as GPUI's hover always did; the frames
// after it re-tint the tagged quads in place (`Scene::animate_color_fades`) with no render, layout or prepaint.

/// A declared hover/active colour transition (seconds, cubic-bezier control points).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorTransition {
    /// Seconds.
    pub duration: f32,
    /// cubic-bezier(x1, y1, x2, y2).
    pub ease: [f32; 4],
}

/// Per-element fade state, kept in the element's interactive state across frames.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ColorFadeState {
    from: (Hsla, Hsla),
    to: (Hsla, Hsla),
    start: std::time::Instant,
    duration: f32,
    ease: [f32; 4],
    hovered: bool,
    pressed: bool,
    base: (Option<Hsla>, Option<Hsla>),
}

/// What one paint of a fading element hands its quads (the window adds the element opacity).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ColorFadePaint {
    from: (Hsla, Hsla),
    to: (Hsla, Hsla),
    start: std::time::Instant,
    duration: f32,
    ease: [f32; 4],
}

/// The tag a fading element's background (kind 0) and border (kind 1) quads carry in the scene.
#[derive(Debug)]
pub(crate) struct ColorFadeTag {
    paint: ColorFadePaint,
    opacity: f32,
    finished: std::sync::atomic::AtomicBool,
}

/// The clock colour fades read: the host's automation clock when it installed one (strips pause and step it).
pub(crate) fn kybern_animation_now() -> std::time::Instant {
    stroke_draw_now()
}

fn color_fade_progress(ease: [f32; 4], start: std::time::Instant, duration: f32, now: std::time::Instant) -> (f32, bool) {
    let x = if duration <= 0.0 { 1.0 } else { (now.saturating_duration_since(start).as_secs_f32() / duration).clamp(0.0, 1.0) };
    let [x1, y1, x2, y2] = ease;
    let at = |t: f32, a: f32, b: f32| 3.0 * a * t * (1.0 - t).powi(2) + 3.0 * b * t * t * (1.0 - t) + t * t * t;
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..20 {
        let mid = (lo + hi) * 0.5;
        if at(mid, x1, x2) < x { lo = mid } else { hi = mid }
    }
    (if x >= 1.0 { 1.0 } else { at((lo + hi) * 0.5, y1, y2) }, x < 1.0)
}

/// CSS colour interpolation: premultiplied sRGB, so a fade from transparent keeps the target's hue.
fn color_fade_mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    if t <= 0.0 { return a; }
    if t >= 1.0 { return b; }
    let (a, b) = (crate::Rgba::from(a), crate::Rgba::from(b));
    let alpha = a.a + (b.a - a.a) * t;
    if alpha <= 0.0 { return Hsla::transparent_black(); }
    let channel = |x: f32, xa: f32, y: f32, ya: f32| ((x * xa) + (y * ya - x * xa) * t) / alpha;
    Hsla::from(crate::Rgba { r: channel(a.r, a.a, b.r, b.a), g: channel(a.g, a.a, b.g, b.a), b: channel(a.b, a.a, b.b, b.a), a: alpha })
}

impl ColorFadePaint {
    pub(crate) fn value(&self, now: std::time::Instant) -> ((Hsla, Hsla), bool) {
        let (t, active) = color_fade_progress(self.ease, self.start, self.duration, now);
        ((color_fade_mix(self.from.0, self.to.0, t), color_fade_mix(self.from.1, self.to.1, t)), active)
    }
}
impl ColorFadeTag {
    /// The quad colours at `now` (with the element opacity) and whether the fade still runs.
    pub(crate) fn value(&self, now: std::time::Instant) -> ((Hsla, Hsla), bool) {
        let ((background, border), active) = self.paint.value(now);
        ((background.opacity(self.opacity), border.opacity(self.opacity)), active)
    }
    pub(crate) fn mark_finished(&self) -> bool {
        !self.finished.swap(true, std::sync::atomic::Ordering::Relaxed)
    }
}

fn solid(fill: Option<&crate::Fill>) -> Option<Option<Hsla>> {
    match fill.and_then(crate::Fill::color) {
        None => Some(None),
        Some(background) if background.tag == crate::color::BackgroundTag::Solid => Some(Some(background.solid)),
        Some(_) => None,
    }
}

/// Decide what an interactive element paints this frame. A flip of its hover (or the release of a press) eases from the
/// colour on screen to the new target; a press lands at once (`:active` feedback is immediate); a change of the
/// element's own rest colours (a selection moved by the keyboard, a theme switch) is instant. `None`: paint as styled.
pub(crate) fn kybern_color_fade(
    transition: ColorTransition,
    slot: &mut Option<ColorFadeState>,
    style: &mut Style,
    base_style: &crate::StyleRefinement,
    hovered: bool,
    pressed: bool,
) -> Option<ColorFadePaint> {
    let Some(background) = solid(style.background.as_ref()) else { *slot = None; return None };
    let Some(base_background) = solid(base_style.background.as_ref()) else { *slot = None; return None };
    let target = (background.unwrap_or_else(Hsla::transparent_black), style.border_color.unwrap_or_else(Hsla::transparent_black));
    let base = (base_background, base_style.border_color);
    let now = kybern_animation_now();
    let state = slot.get_or_insert(ColorFadeState { from: target, to: target, start: now, duration: 0.0, ease: transition.ease, hovered, pressed, base });
    if state.to != target {
        let instant = state.base != base || (pressed && !state.pressed) || (hovered == state.hovered && pressed == state.pressed);
        let (shown, _) = ColorFadePaint { from: state.from, to: state.to, start: state.start, duration: state.duration, ease: state.ease }.value(now);
        *state = ColorFadeState {
            from: if instant { target } else { shown },
            to: target,
            // The first step shows on the frame the flip renders, like a paint-only entrance on its mount frame.
            start: now.checked_sub(std::time::Duration::from_micros(16_667)).unwrap_or(now),
            duration: if instant { 0.0 } else { transition.duration },
            ease: transition.ease,
            hovered,
            pressed,
            base,
        };
    } else {
        state.hovered = hovered;
        state.pressed = pressed;
        state.base = base;
    }
    let paint = ColorFadePaint { from: state.from, to: state.to, start: state.start, duration: state.duration, ease: state.ease };
    let ((background, border), active) = paint.value(now);
    if !active {
        return None;
    }
    // Keep both quads in the scene for the whole fade, even while a colour is transparent (GPUI skips those).
    let keep = |mut color: Hsla| { if color.a <= 0.0 { color.a = f32::EPSILON } color };
    style.background = Some(crate::Fill::Color(keep(background).into()));
    if style.border_widths.any(|width| !width.is_zero()) {
        style.border_color = Some(keep(border));
    }
    Some(paint)
}

impl Window {
    /// `Style::paint` with the element's background quad (and, after its children, border quads) tagged for a colour fade.
    pub(crate) fn kybern_paint_style_with_fade(
        &mut self,
        style: &Style,
        fade: Option<ColorFadePaint>,
        bounds: Bounds<Pixels>,
        cx: &mut App,
        continuation: impl FnOnce(&mut Window, &mut App),
    ) {
        let Some(paint) = fade else { return style.paint(bounds, self, cx, continuation) };
        let tag = std::sync::Arc::new(ColorFadeTag { paint, opacity: self.element_opacity(), finished: Default::default() });
        let previous = self.next_frame.scene.active_color_fade.replace((tag.clone(), 0));
        style.paint(bounds, self, cx, |window, cx| {
            window.next_frame.scene.active_color_fade = None;
            continuation(window, cx);
            window.next_frame.scene.active_color_fade = Some((tag, 1));
        });
        self.next_frame.scene.active_color_fade = previous;
    }
}
