// A Q10C picture sampled in the decoder's buffer, for scene.wgsl (build.rs
// joins them into scene_tp10): each plane through TP10 texture descriptors
// (src/q10c.rs, src/tp10_eyes.rs). One descriptor can't hold a whole 8K
// plane, so each plane is read in overlapping bands of rows, and a sample
// comes from the band whose middle is nearest: its bilinear neighbours are
// then always in the same band. TP10 gives 10-bit values, not P010's.

@group(0) @binding(2) var luma0: texture_2d<f32>;
@group(0) @binding(3) var luma1: texture_2d<f32>;
@group(0) @binding(4) var luma2: texture_2d<f32>;
@group(0) @binding(5) var luma3: texture_2d<f32>;
@group(0) @binding(6) var luma4: texture_2d<f32>;
@group(0) @binding(7) var luma5: texture_2d<f32>;
@group(0) @binding(8) var luma6: texture_2d<f32>;
@group(0) @binding(9) var luma7: texture_2d<f32>;
@group(0) @binding(10) var chroma0: texture_2d<f32>;
@group(0) @binding(11) var chroma1: texture_2d<f32>;
@group(0) @binding(12) var chroma2: texture_2d<f32>;
@group(0) @binding(13) var chroma3: texture_2d<f32>;

// Where `uv` falls in a plane read in bands `b` (Params.tp10): the
// coordinates in its band, and the band.
fn in_band(uv: vec2<f32>, b: vec4<f32>) -> vec3<f32> {
    let y = uv.y * b.z;
    let k = clamp(round((y - 0.5 * b.y) / b.x), 0.0, b.w - 1.0);
    let row0 = k * b.x;
    return vec3<f32>(uv.x, (y - row0) / min(b.y, b.z - row0), k);
}

fn luma_tp10(uv: vec2<f32>) -> f32 {
    let p = in_band(uv, params.tp10);
    switch u32(p.z) {
        case 0u: { return textureSampleLevel(luma0, samp, p.xy, 0.0).r; }
        case 1u: { return textureSampleLevel(luma1, samp, p.xy, 0.0).r; }
        case 2u: { return textureSampleLevel(luma2, samp, p.xy, 0.0).r; }
        case 3u: { return textureSampleLevel(luma3, samp, p.xy, 0.0).r; }
        case 4u: { return textureSampleLevel(luma4, samp, p.xy, 0.0).r; }
        case 5u: { return textureSampleLevel(luma5, samp, p.xy, 0.0).r; }
        case 6u: { return textureSampleLevel(luma6, samp, p.xy, 0.0).r; }
        default: { return textureSampleLevel(luma7, samp, p.xy, 0.0).r; }
    }
}

fn chroma_tp10(uv: vec2<f32>) -> vec2<f32> {
    let p = in_band(uv, params.tp10_uv);
    switch u32(p.z) {
        case 0u: { return textureSampleLevel(chroma0, samp, p.xy, 0.0).rg; }
        case 1u: { return textureSampleLevel(chroma1, samp, p.xy, 0.0).rg; }
        case 2u: { return textureSampleLevel(chroma2, samp, p.xy, 0.0).rg; }
        default: { return textureSampleLevel(chroma3, samp, p.xy, 0.0).rg; }
    }
}

fn sample_yuv(uv: vec2<f32>) -> vec3<f32> {
    let yuv = vec3<f32>(luma_tp10(uv), chroma_tp10(uv)) * params.yuv_offset.w - params.yuv_offset.xyz;
    return vec3<f32>(dot(params.yuv_r.xyz, yuv), dot(params.yuv_g.xyz, yuv), dot(params.yuv_b.xyz, yuv));
}

fn luma_at(uv: vec2<f32>) -> f32 {
    return luma_tp10(uv) * params.yuv_offset.w;
}
