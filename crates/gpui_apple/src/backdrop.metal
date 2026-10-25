#include <metal_stdlib>
using namespace metal;
// Coordinates are device pixels. Two separable Gaussian passes, 49 samples
// each. Crop includes the 3-sigma halo. No read/write feedback texture.
struct Params { float4 crop; float4 bounds; float4 clip; float4 values; float4 viewport; uint rounded_clip; float scale; };
struct RoundedClipNode { float bounds[4]; float radii[4]; uint parent; uint kind; };
// Edge fade link (N3), as in shaders.metal: ramps top, right, bottom, left.
float backdrop_edge_fade(float2 p, RoundedClipNode n) {
    float2 lo=float2(n.bounds[0],n.bounds[1]),size=float2(n.bounds[2],n.bounds[3]);
    float4 band=float4(n.radii[0],n.radii[1],n.radii[2],n.radii[3]);
    float4 d=float4(p.y-lo.y,lo.x+size.x-p.x,lo.y+size.y-p.y,p.x-lo.x);
    float4 r=select(float4(1.),saturate(d/max(band,float4(1e-4))),band>0.);
    float m=min(min(r.x,r.y),min(r.z,r.w));
    return m*m;
}
// Specialised per pipeline (index 0): true on Apple GPUs, which read the target in the shader (programmable blending); false elsewhere,
// where the fixed-function blend of the vertical pass is used instead and the destination alpha stays as it was.
constant bool programmable_blend [[function_constant(0)]];
struct Varying { float4 position [[position]]; float2 pixel; };
vertex Varying backdrop_vertex(uint i [[vertex_id]], constant Params &p [[buffer(0)]]) {
    float2 uv=float2((i==1||i==3)?1:0, i>=2?1:0);
    float2 pos = p.values.w==0 ? uv*p.crop.zw : p.bounds.xy+uv*p.bounds.zw;
    Varying o; o.pixel=pos;
    o.position=float4(pos/p.viewport.xy*float2(2,-2)+float2(-1,1),0,1); return o;
}
fragment float4 backdrop_fragment(Varying in [[stage_in]], float4 dst [[color(0), function_constant(programmable_blend)]], texture2d<float> source [[texture(0)]], constant Params &p [[buffer(0)]], device const RoundedClipNode *clips [[buffer(7)]]) {
    constexpr sampler s(coord::pixel, address::clamp_to_edge, filter::linear);
    bool vertical=p.values.w!=0;
    float2 pos=vertical?in.pixel-p.crop.xy:in.pixel+p.crop.xy;
    float sigma=p.values.x;
    float4 sum=0; float total=0;
    for(int i=-24;i<=24;i++) {
        float d=float(i)*max(sigma/8.0,0.125);
        float w=exp(-0.5*d*d/max(sigma*sigma,0.001));
        float2 at=pos+(vertical?float2(0,d):float2(d,0));
        if(vertical)at=clamp(at,float2(0.5),p.crop.zw-float2(0.5));
        sum+=source.sample(s,at)*w; total+=w;
    }
    float4 c=sum/total;
    if(vertical){
        // The blurred alpha, kept apart from the coverage mask below: on a translucent window the surface's own alpha must be
        // the blurred scene's, or the scene's silhouettes (opaque glyph pixels on a translucent coat) stay sharp in the alpha
        // channel and the desktop shows through them however well the colour is blurred.
        float blurred_alpha=c.a;
        if(any(in.pixel<p.clip.xy)||any(in.pixel>=p.clip.xy+p.clip.zw))discard_fragment();
        float radius=min(p.values.z,min(p.bounds.z,p.bounds.w)/2);
        float2 q=abs(in.pixel-(p.bounds.xy+p.bounds.zw/2))-(p.bounds.zw/2-radius);
        float distance=length(max(q,0.0))+min(max(q.x,q.y),0.0)-radius;
        if(distance>=0.5)discard_fragment();
        float luminance=dot(c.rgb,float3(0.2126,0.7152,0.0722));
        c.rgb=mix(float3(luminance),c.rgb,p.values.y);
        // Blend fractional rounded edge against the unfiltered destination.
        c.a=clamp(0.5-distance,0.0,1.0)*p.viewport.z;
        uint id=p.rounded_clip;
        while(id!=0){
            RoundedClipNode n=clips[id];
            if(n.kind!=0){c.a*=backdrop_edge_fade(in.pixel,n);id=n.parent;continue;}
            float2 lo=float2(n.bounds[0],n.bounds[1]),size=float2(n.bounds[2],n.bounds[3]);
            bool right=in.pixel.x>=lo.x+size.x/2.,bottom=in.pixel.y>=lo.y+size.y/2.;
            float r=n.radii[bottom?(right?2:3):(right?1:0)];
            float2 center=lo+float2(right?size.x-r:r,bottom?size.y-r:r);
            float2 q=float2(right?in.pixel.x-center.x:center.x-in.pixel.x,bottom?in.pixel.y-center.y:center.y-in.pixel.y);
            float d=length(max(q,0.))+min(max(q.x,q.y),0.)-r;
            if(any(in.pixel<lo)||any(in.pixel>=lo+size)||d>=.5)discard_fragment();
            c.a*=saturate(.5-d); id=n.parent;
        }
        if(p.viewport.w>0)c.a*=clamp((in.pixel.y-p.bounds.y)/(p.bounds.w*p.viewport.w),0.0,1.0);
        // Programmable blending (Apple GPUs; the pass then has no fixed-function blend): the blurred premultiplied colour and the
        // blurred alpha replace the destination by the coverage, `mix(dst, blurred, coverage)` on all four channels.
        if(programmable_blend)return float4(c.rgb*c.a+dst.rgb*(1.0-c.a),blurred_alpha*c.a+dst.a*(1.0-c.a));
    }
    return c;
}
