// Kybern: the scene backdrop filter (gpui_apple/src/backdrop.metal), in two passes over a copy of the target region
// behind the filter. Coordinates are device pixels. The horizontal pass writes `crop / scale` texels; the vertical pass
// draws the clipped bounds into the target. The copy also stands in for Metal's programmable blending: the vertical pass
// writes `mix(destination, blurred, coverage)` on all four channels, so on a translucent window the alpha is blurred with
// the colour (ADE-58) without reading the render target.

struct Params {
    // [x, y, width, height] of the copied region.
    crop: vec4<f32>,
    bounds: vec4<f32>,
    // The bounds intersected with the content mask.
    clip: vec4<f32>,
    // sigma, saturation, corner radius, scale (1, or 2 for the half-resolution path).
    values: vec4<f32>,
    // Target width, height, opacity, fade-top fraction.
    viewport: vec4<f32>,
    // Rounded clip chain, tap count, 0, 0.
    info: vec4<u32>,
    // [offset, weight, 0, 0] (gpui::backdrop_kernel::BackdropKernel).
    taps: array<vec4<f32>, 49>,
}

// `gpui::RoundedClip`, 40 bytes: a rounded clip (`kind` 0) or an edge fade (1).
struct RoundedClipNode {
    bounds: array<f32, 4>,
    radii: array<f32, 4>,
    parent: u32,
    kind: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> rounded_clips: array<RoundedClipNode>;
// The horizontal pass reads the copy; the vertical pass reads the horizontal pass's output.
@group(1) @binding(0) var source: texture_2d<f32>;
// The copied target region, before this filter: the destination the vertical pass blends with.
@group(1) @binding(1) var destination: texture_2d<f32>;
@group(1) @binding(2) var linear_clamp: sampler;

struct Varying {
    @builtin(position) position: vec4<f32>,
}

fn corner(vertex_id: u32) -> vec2<f32> {
    return vec2<f32>(f32(vertex_id & 1u), f32(vertex_id >> 1u));
}

@vertex
fn vs_backdrop_horizontal(@builtin(vertex_index) vertex_id: u32) -> Varying {
    // The pass's viewport is the scratch region in use.
    let unit = corner(vertex_id);
    var out: Varying;
    out.position = vec4<f32>(unit.x * 2.0 - 1.0, 1.0 - unit.y * 2.0, 0.0, 1.0);
    return out;
}

@fragment
fn fs_backdrop_horizontal(input: Varying) -> @location(0) vec4<f32> {
    let k = params.values.w;
    let source_size = vec2<f32>(textureDimensions(source));
    // A half-resolution texel samples the centre of its 2x2 block of the copy.
    let position = input.position.xy * k;
    var color = vec4<f32>(0.0);
    for (var i = 0u; i < params.info.y; i++) {
        let tap = params.taps[i];
        // Metal samples the whole target here; the copy ends at the crop, which equals it for every texel the vertical
        // pass reads inside the clip (the crop holds the 3-sigma halo), and at a target edge equals clamp-to-edge. The
        // pooled copy texture is larger than the crop: an odd crop's last half block reads its one copied column or row.
        let at = clamp(position + vec2<f32>(tap.x * k, 0.0), vec2<f32>(0.5), params.crop.zw - vec2<f32>(0.5));
        color += textureSampleLevel(source, linear_clamp, at / source_size, 0.0) * tap.y;
    }
    return color;
}

@vertex
fn vs_backdrop_vertical(@builtin(vertex_index) vertex_id: u32) -> Varying {
    let pixel = params.clip.xy + corner(vertex_id) * params.clip.zw;
    var out: Varying;
    out.position = vec4<f32>(pixel / params.viewport.xy * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    return out;
}

// Edge fade link (N3), as in backdrop.metal: ramps top, right, bottom, left.
fn edge_fade(p: vec2<f32>, node: RoundedClipNode) -> f32 {
    let lo = vec2<f32>(node.bounds[0], node.bounds[1]);
    let size = vec2<f32>(node.bounds[2], node.bounds[3]);
    let band = vec4<f32>(node.radii[0], node.radii[1], node.radii[2], node.radii[3]);
    let d = vec4<f32>(p.y - lo.y, lo.x + size.x - p.x, lo.y + size.y - p.y, p.x - lo.x);
    let r = select(vec4<f32>(1.0), saturate(d / max(band, vec4<f32>(1e-4))), band > vec4<f32>(0.0));
    let m = min(min(r.x, r.y), min(r.z, r.w));
    return m * m;
}

@fragment
fn fs_backdrop_vertical(input: Varying) -> @location(0) vec4<f32> {
    let pixel = input.position.xy;
    if (any(pixel < params.clip.xy) || any(pixel >= params.clip.xy + params.clip.zw)) {
        discard;
    }
    let k = params.values.w;
    let source_size = vec2<f32>(textureDimensions(source));
    let position = (pixel - params.crop.xy) / k;
    // The horizontal pass wrote `ceil(crop / scale)` texels (`BackdropPlan::scratch_size`).
    let limit = ceil(params.crop.zw / k) - vec2<f32>(0.5);
    var c = vec4<f32>(0.0);
    for (var i = 0u; i < params.info.y; i++) {
        let tap = params.taps[i];
        let at = clamp(position + vec2<f32>(0.0, tap.x), vec2<f32>(0.5), limit);
        c += textureSampleLevel(source, linear_clamp, at / source_size, 0.0) * tap.y;
    }
    // The blurred alpha, kept apart from the coverage below (backdrop.metal, ADE-58).
    let blurred_alpha = c.a;
    let radius = min(params.values.z, min(params.bounds.z, params.bounds.w) / 2.0);
    let q = abs(pixel - (params.bounds.xy + params.bounds.zw / 2.0)) - (params.bounds.zw / 2.0 - radius);
    let distance = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - radius;
    if (distance >= 0.5) {
        discard;
    }
    let luminance = dot(c.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let rgb = mix(vec3<f32>(luminance), c.rgb, params.values.y);
    var coverage = clamp(0.5 - distance, 0.0, 1.0) * params.viewport.z;
    var id = params.info.x;
    for (var link = 0u; link < 64u && id != 0u; link++) {
        let node = rounded_clips[id];
        if (node.kind != 0u) {
            coverage *= edge_fade(pixel, node);
            id = node.parent;
            continue;
        }
        let lo = vec2<f32>(node.bounds[0], node.bounds[1]);
        let size = vec2<f32>(node.bounds[2], node.bounds[3]);
        let right = pixel.x >= lo.x + size.x / 2.0;
        let bottom = pixel.y >= lo.y + size.y / 2.0;
        let r = select(select(node.radii[0], node.radii[1], right), select(node.radii[3], node.radii[2], right), bottom);
        let center = lo + vec2<f32>(select(r, size.x - r, right), select(r, size.y - r, bottom));
        let d2 = vec2<f32>(select(center.x - pixel.x, pixel.x - center.x, right), select(center.y - pixel.y, pixel.y - center.y, bottom));
        let d = length(max(d2, vec2<f32>(0.0))) + min(max(d2.x, d2.y), 0.0) - r;
        if (any(pixel < lo) || any(pixel >= lo + size) || d >= 0.5) {
            discard;
        }
        coverage *= saturate(0.5 - d);
        id = node.parent;
    }
    if (params.viewport.w > 0.0) {
        coverage *= clamp((pixel.y - params.bounds.y) / (params.bounds.w * params.viewport.w), 0.0, 1.0);
    }
    let destination_color = textureLoad(destination, vec2<i32>(floor(pixel - params.crop.xy)), 0);
    return vec4<f32>(rgb * coverage + destination_color.rgb * (1.0 - coverage), blurred_alpha * coverage + destination_color.a * (1.0 - coverage));
}
