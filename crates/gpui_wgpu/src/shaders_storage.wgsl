// Storage-buffer instance transport. Concatenated after `shaders.wgsl` on backends
// with storage buffer support; `shaders_webgl.wgsl` is the WebGL2 counterpart that
// decodes the same records from a texture instead.
//
// All buffers share `@group(1) @binding(0)` because each pipeline binds only the
// buffer its own entry points read.

@group(1) @binding(0) var<storage, read> b_quads: array<Quad>;
@group(1) @binding(0) var<storage, read> b_shadows: array<Shadow>;
@group(1) @binding(0) var<storage, read> b_path_vertices: array<PathRasterizationVertex>;
@group(1) @binding(0) var<storage, read> b_path_sprites: array<PathSprite>;
@group(1) @binding(0) var<storage, read> b_underlines: array<Underline>;
@group(1) @binding(0) var<storage, read> b_mono_sprites: array<MonochromeSprite>;
@group(1) @binding(0) var<storage, read> b_poly_sprites: array<PolychromeSprite>;
@group(1) @binding(0) var<storage, read> b_surfaces: array<SurfaceInstance>;

fn load_quad(instance_id: u32) -> Quad {
    return b_quads[instance_id];
}

fn load_shadow(instance_id: u32) -> Shadow {
    return b_shadows[instance_id];
}

fn load_path_vertex(vertex_id: u32) -> PathRasterizationVertex {
    return b_path_vertices[vertex_id];
}

fn load_path_sprite(instance_id: u32) -> PathSprite {
    return b_path_sprites[instance_id];
}

fn load_underline(instance_id: u32) -> Underline {
    return b_underlines[instance_id];
}

fn load_mono_sprite(instance_id: u32) -> MonochromeSprite {
    return b_mono_sprites[instance_id];
}

fn load_poly_sprite(instance_id: u32) -> PolychromeSprite {
    return b_poly_sprites[instance_id];
}

fn load_surface(instance_id: u32) -> SurfaceInstance {
    return b_surfaces[instance_id];
}

// Kybern: the scene's rounded clip chains (rounded_clip_gpui.py), shared by every pipeline.
@group(1) @binding(1) var<storage, read> b_rounded_clips: array<RoundedClipNode>;

// Edge fade link (shaders.metal): ramps are top, right, bottom, left; 0 leaves that edge alone.
fn edge_fade_alpha(p: vec2<f32>, lo: vec2<f32>, size: vec2<f32>, band: vec4<f32>) -> f32 {
    let d = vec4<f32>(p.y - lo.y, lo.x + size.x - p.x, lo.y + size.y - p.y, p.x - lo.x);
    let r = select(vec4<f32>(1.0), saturate(d / max(band, vec4<f32>(1e-4))), band > vec4<f32>(0.0));
    let m = min(min(r.x, r.y), min(r.z, r.w));
    return m * m;
}

// Coverage of `p` inside rounded chain `id` (0 = rectangular): interior pixels
// take comparisons only; corner pixels evaluate one circle distance per link.
// Edge fades multiply, so nested scopes compound (shaders.metal).
fn rounded_clip_alpha(p: vec2<f32>, chain: u32) -> f32 {
    var id = chain;
    var coverage = 1.0;
    var fade = 1.0;
    loop {
        if (id == 0u || id >= arrayLength(&b_rounded_clips)) {
            break;
        }
        let n = b_rounded_clips[id];
        let lo = vec2<f32>(n.bounds[0], n.bounds[1]);
        let size = vec2<f32>(n.bounds[2], n.bounds[3]);
        if (n.kind != 0u) {
            fade = fade * edge_fade_alpha(p, lo, size, vec4<f32>(n.radii[0], n.radii[1], n.radii[2], n.radii[3]));
            id = n.parent;
            continue;
        }
        let right = p.x >= lo.x + size.x / 2.0;
        let bottom = p.y >= lo.y + size.y / 2.0;
        var r = n.radii[0];
        if (right && !bottom) { r = n.radii[1]; }
        if (right && bottom) { r = n.radii[2]; }
        if (!right && bottom) { r = n.radii[3]; }
        let c = lo + vec2<f32>(select(r, size.x - r, right), select(r, size.y - r, bottom));
        let q = vec2<f32>(select(c.x - p.x, p.x - c.x, right), select(c.y - p.y, p.y - c.y, bottom));
        let distance = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
        if (any(p < lo) || any(p >= lo + size) || distance >= 0.5) {
            return 0.0;
        }
        coverage = min(coverage, saturate(0.5 - distance));
        id = n.parent;
    }
    if (fade <= 0.0) {
        return 0.0;
    }
    return coverage * fade;
}
