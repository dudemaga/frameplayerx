// The video as uploaded or converted planes (I420, NV12 or P010), for
// scene.wgsl (build.rs joins them).

@group(0) @binding(2) var plane0: texture_2d<f32>;
@group(0) @binding(3) var plane1: texture_2d<f32>;
@group(0) @binding(4) var plane2: texture_2d<f32>;

fn sample_yuv(uv: vec2<f32>) -> vec3<f32> {
    let y = textureSampleLevel(plane0, samp, uv, 0.0).r;
    var c: vec2<f32>;
    if (params.mode.w == 0u) {
        c = vec2<f32>(textureSampleLevel(plane1, samp, uv, 0.0).r, textureSampleLevel(plane2, samp, uv, 0.0).r);
    } else {
        c = textureSampleLevel(plane1, samp, uv, 0.0).rg;
    }
    let yuv = vec3<f32>(y, c) * params.yuv_offset.w - params.yuv_offset.xyz;
    return vec3<f32>(dot(params.yuv_r.xyz, yuv), dot(params.yuv_g.xyz, yuv), dot(params.yuv_b.xyz, yuv));
}

fn luma_at(uv: vec2<f32>) -> f32 {
    return textureSampleLevel(plane0, samp, uv, 0.0).r * params.yuv_offset.w;
}
