// Counts the brightness of an HDR frame's samples (every `step`th luma
// texel each way) into a histogram of the PQ-coded brightest channel, as
// the scene shader tone maps it; src/peak.rs reads it.

struct Measure {
    // YUV -> R'G'B' as the scene shader's (src/params.rs: yuv_matrix).
    yuv_r: vec4<f32>,
    yuv_g: vec4<f32>,
    yuv_b: vec4<f32>,
    yuv_offset: vec4<f32>,
    // x: planes (0 I420, 1 NV12/P010), y: transfer (1 PQ, 2 HLG),
    // z: BT.2020 primaries, w: grid step in luma texels
    mode: vec4<u32>,
};
var<immediate> m: Measure;

const BINS: u32 = 128u;

@group(0) @binding(0) var<storage, read_write> hist: array<atomic<u32>, BINS>;
@group(0) @binding(1) var plane0: texture_2d<f32>;
@group(0) @binding(2) var plane1: texture_2d<f32>;
@group(0) @binding(3) var plane2: texture_2d<f32>;

var<workgroup> local: array<atomic<u32>, BINS>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) id: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    if (li < BINS) { atomicStore(&local[li], 0u); }
    workgroupBarrier();
    let size = textureDimensions(plane0);
    let step = m.mode.w;
    let p = id.xy * step + vec2<u32>(step / 2u);
    if (all(p < size)) {
        let y = textureLoad(plane0, p, 0).r;
        let cp = min(p / 2u, textureDimensions(plane1) - 1u);
        var c: vec2<f32>;
        if (m.mode.x == 0u) {
            c = vec2<f32>(textureLoad(plane1, cp, 0).r, textureLoad(plane2, cp, 0).r);
        } else {
            c = textureLoad(plane1, cp, 0).rg;
        }
        let yuv = vec3<f32>(y, c) * m.yuv_offset.w - m.yuv_offset.xyz;
        let rgb = vec3<f32>(dot(m.yuv_r.xyz, yuv), dot(m.yuv_g.xyz, yuv), dot(m.yuv_b.xyz, yuv));
        let nits = hdr_nits(rgb, m.mode.y, m.mode.z != 0u);
        let bin = min(u32(pq_oetf1(max3(nits)) * f32(BINS)), BINS - 1u);
        atomicAdd(&local[bin], 1u);
    }
    workgroupBarrier();
    if (li < BINS) {
        let n = atomicLoad(&local[li]);
        if (n > 0u) { atomicAdd(&hist[li], n); }
    }
}
