#include <metal_stdlib>
using namespace metal;
struct RegionVertex { float4 position [[position]]; };
vertex RegionVertex region_vertex(uint index [[vertex_id]]) {
    float2 p = index == 0 ? float2(-1,-1) : index == 1 ? float2(3,-1) : float2(-1,3);
    return {float4(p,0,1)};
}
fragment float4 region_clear(RegionVertex in [[stage_in]], constant float &alpha [[buffer(0)]]) {
    return float4(0,0,0,alpha);
}
fragment float4 region_copy(RegionVertex in [[stage_in]], texture2d<float, access::read> source [[texture(0)]], constant uint2 &origin [[buffer(1)]]) {
    return source.read(uint2(in.position.xy)+origin);
}

fragment float4 region_copy_holes(RegionVertex in [[stage_in]], texture2d<float, access::read> source [[texture(0)]], constant uint4 *holes [[buffer(2)]], constant uint &count [[buffer(3)]]) {
    uint2 p=uint2(in.position.xy);
    for(uint i=0;i<count;i++) {
        uint4 r=holes[i];
        if(p.x>=r.x && p.y>=r.y && p.x<r.z && p.y<r.w) return float4(0);
    }
    return source.read(p);
}
