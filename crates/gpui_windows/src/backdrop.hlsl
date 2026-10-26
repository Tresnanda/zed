// Kybern: the scene backdrop filter (gpui_apple/src/backdrop.metal; gpui_wgpu/src/backdrop.wgsl is the same algorithm),
// in two passes over a copy of the back buffer region behind the filter. Device pixels. The horizontal pass writes
// `crop / scale` texels; the vertical pass draws the clip into the back buffer and writes `lerp(destination, blurred,
// coverage)` on all four channels, so on a composition swap chain the alpha is blurred with the colour (ADE-58).
// vs_4_1 / ps_4_1, like shaders.hlsl.

cbuffer BackdropParams: register(b2) {
    // [x, y, width, height] of the copied region.
    float4 crop;
    float4 bounds;
    // The bounds intersected with the content mask.
    float4 clip;
    // sigma, saturation, corner radius, scale (1, or 2 for the half-resolution path).
    float4 values;
    // Target width, height, opacity, fade-top fraction.
    float4 target_info;
    // Rounded clip chain, tap count, 0, 0.
    uint4 info;
    // [offset, weight, 0, 0] (gpui::backdrop_kernel::BackdropKernel).
    float4 taps[49];
};

// `gpui::RoundedClip`, 40 bytes (the buffer's stride), bound at t2 for every pass by the renderer.
struct RoundedClipNode {
    float4 bounds;
    float4 radii;
    uint parent;
    uint kind;
};
StructuredBuffer<RoundedClipNode> rounded_clips: register(t2);
// The horizontal pass reads the copy; the vertical pass reads the horizontal pass's output.
Texture2D<float4> backdrop_source: register(t3);
// The copied back buffer region, before this filter: the destination the vertical pass blends with.
Texture2D<float4> backdrop_destination: register(t4);
SamplerState backdrop_sampler: register(s1);

float2 corner(uint vertex_id) {
    return float2(float(vertex_id & 1u), float(vertex_id >> 1u));
}

float4 backdrop_horizontal_vertex(uint vertex_id: SV_VertexID): SV_Position {
    // The pass's viewport is the scratch region in use.
    float2 unit = corner(vertex_id);
    return float4(unit.x * 2.0 - 1.0, 1.0 - unit.y * 2.0, 0.0, 1.0);
}

float4 backdrop_horizontal_fragment(float4 position: SV_Position): SV_Target {
    float k = values.w;
    float width, height;
    backdrop_source.GetDimensions(width, height);
    float2 source_size = float2(width, height);
    // A half-resolution texel samples the centre of its 2x2 block of the copy.
    float2 p = position.xy * k;
    float4 color = float4(0.0, 0.0, 0.0, 0.0);
    [loop]
    for (uint i = 0; i < info.y; i++) {
        // The copy ends at the crop, odd sizes included; see backdrop.wgsl.
        float2 at = clamp(p + float2(taps[i].x * k, 0.0), float2(0.5, 0.5), crop.zw - 0.5);
        color += backdrop_source.SampleLevel(backdrop_sampler, at / source_size, 0.0) * taps[i].y;
    }
    return color;
}

float4 backdrop_vertical_vertex(uint vertex_id: SV_VertexID): SV_Position {
    float2 pixel = clip.xy + corner(vertex_id) * clip.zw;
    return float4(pixel / target_info.xy * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}

// Edge fade link (N3), as in backdrop.metal: ramps top, right, bottom, left.
float backdrop_edge_fade(float2 p, RoundedClipNode n) {
    float4 d = float4(p.y - n.bounds.y, n.bounds.x + n.bounds.z - p.x, n.bounds.y + n.bounds.w - p.y, p.x - n.bounds.x);
    float4 r = n.radii > 0.0 ? saturate(d / max(n.radii, 1e-4)) : float4(1.0, 1.0, 1.0, 1.0);
    float m = min(min(r.x, r.y), min(r.z, r.w));
    return m * m;
}

float4 backdrop_vertical_fragment(float4 position: SV_Position): SV_Target {
    float2 pixel = position.xy;
    if (any(pixel < clip.xy) || any(pixel >= clip.xy + clip.zw)) {
        discard;
    }
    float k = values.w;
    float width, height;
    backdrop_source.GetDimensions(width, height);
    float2 source_size = float2(width, height);
    float2 p = (pixel - crop.xy) / k;
    // The horizontal pass wrote `ceil(crop / scale)` texels (`BackdropPlan::scratch_size`).
    float2 limit = ceil(crop.zw / k) - 0.5;
    float4 c = float4(0.0, 0.0, 0.0, 0.0);
    [loop]
    for (uint i = 0; i < info.y; i++) {
        float2 at = clamp(p + float2(0.0, taps[i].x), float2(0.5, 0.5), limit);
        c += backdrop_source.SampleLevel(backdrop_sampler, at / source_size, 0.0) * taps[i].y;
    }
    // The blurred alpha, kept apart from the coverage below (backdrop.metal, ADE-58).
    float blurred_alpha = c.a;
    float radius = min(values.z, min(bounds.z, bounds.w) / 2.0);
    float2 q = abs(pixel - (bounds.xy + bounds.zw / 2.0)) - (bounds.zw / 2.0 - radius);
    float distance = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - radius;
    if (distance >= 0.5) {
        discard;
    }
    float luminance = dot(c.rgb, float3(0.2126, 0.7152, 0.0722));
    float3 rgb = lerp(float3(luminance, luminance, luminance), c.rgb, values.y);
    float coverage = saturate(0.5 - distance) * target_info.z;
    uint id = info.x;
    [loop]
    for (uint link = 0; link < 64 && id != 0; link++) {
        RoundedClipNode n = rounded_clips[id];
        if (n.kind != 0) {
            coverage *= backdrop_edge_fade(pixel, n);
            id = n.parent;
            continue;
        }
        float2 lo = n.bounds.xy;
        float2 size = n.bounds.zw;
        bool right = pixel.x >= lo.x + size.x / 2.0;
        bool bottom = pixel.y >= lo.y + size.y / 2.0;
        float r = bottom ? (right ? n.radii.z : n.radii.w) : (right ? n.radii.y : n.radii.x);
        float2 center = lo + float2(right ? size.x - r : r, bottom ? size.y - r : r);
        float2 d2 = float2(right ? pixel.x - center.x : center.x - pixel.x, bottom ? pixel.y - center.y : center.y - pixel.y);
        float d = length(max(d2, 0.0)) + min(max(d2.x, d2.y), 0.0) - r;
        if (any(pixel < lo) || any(pixel >= lo + size) || d >= 0.5) {
            discard;
        }
        coverage *= saturate(0.5 - d);
        id = n.parent;
    }
    if (target_info.w > 0.0) {
        coverage *= saturate((pixel.y - bounds.y) / (bounds.w * target_info.w));
    }
    float4 destination = backdrop_destination.Load(int3(int2(floor(pixel - crop.xy)), 0));
    return float4(rgb * coverage + destination.rgb * (1.0 - coverage), blurred_alpha * coverage + destination.a * (1.0 - coverage));
}
