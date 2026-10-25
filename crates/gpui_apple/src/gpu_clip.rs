// Included in gpui_apple::metal_renderer. Simplification touches only the
// transient GPU upload; retained masks stay intact for replay and transforms.
include!("gpu_clip_bounds.rs");
fn gpu_clip_prune(scene: &Scene, mut mask: ContentMask<ScaledPixels>, bounds: Bounds<ScaledPixels>) -> ContentMask<ScaledPixels> {
    // Streaming/backdrop scenes showed inconsistent AGX work across repeated
    // pairs. Keep their uploaded masks identical to the original renderer until
    // blur/encoder dependencies have a separate, reproducible optimization.
    if !scene.backdrops.is_empty() { return mask; }
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if mask.rounded_clip == 0 || !*ENABLED.get_or_init(|| std::env::var("KYBERN_GPU_CLIP_PRUNE").as_deref() != Ok("0")) { return mask; }
    let bounds=bounds.intersect(&mask.bounds);
    let rect=[bounds.origin.x.0,bounds.origin.y.0,bounds.size.width.0,bounds.size.height.0];
    let mut id=mask.rounded_clip;
    while id!=0 {
        let Some(clip)=scene.rounded_clips.get(id as usize) else { return mask; };
        // An edge fade (N3) is skipped only where it multiplies by exactly 1.
        let inside = if clip.kind != 0 { gpui::edge_fade_interior(rect,clip.bounds,clip.radii) } else { clip_contains_rect(rect,clip.bounds,clip.radii) };
        if !inside { return mask; }
        id=clip.parent;
    }
    mask.rounded_clip=0;
    mask
}
