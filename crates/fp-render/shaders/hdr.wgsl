// HDR maths shared by scene.wgsl and peak.wgsl (build.rs puts this file in
// front of each). The same maths in Rust is src/hdr.rs, which the tests
// check these against.

const PQ_M1: f32 = 0.1593017578125;
const PQ_M2: f32 = 78.84375;
const PQ_C1: f32 = 0.8359375;
const PQ_C2: f32 = 18.8515625;
const PQ_C3: f32 = 18.6875;

// Nits of PQ codes.
fn pq_eotf(e: vec3<f32>) -> vec3<f32> {
    let p = pow(clamp(e, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / PQ_M2));
    return pow(max(p - PQ_C1, vec3<f32>(0.0)) / (PQ_C2 - PQ_C3 * p), vec3<f32>(1.0 / PQ_M1)) * 10000.0;
}

fn pq_eotf1(e: f32) -> f32 {
    let p = pow(clamp(e, 0.0, 1.0), 1.0 / PQ_M2);
    return pow(max(p - PQ_C1, 0.0) / (PQ_C2 - PQ_C3 * p), 1.0 / PQ_M1) * 10000.0;
}

// PQ code of nits.
fn pq_oetf1(nits: f32) -> f32 {
    let y = pow(max(nits, 0.0) / 10000.0, PQ_M1);
    return pow((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y), PQ_M2);
}

fn hlg_inverse_oetf(e: vec3<f32>) -> vec3<f32> {
    let a = 0.17883277; let b = 0.28466892; let c = 0.55991073;
    var o: vec3<f32>;
    for (var i = 0; i < 3; i++) {
        let x = e[i];
        if (x <= 0.5) { o[i] = x * x / 3.0; } else { o[i] = (exp((x - c) / a) + b) / 12.0; }
    }
    return o;
}

fn bt2020_to_709(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        dot(vec3<f32>(1.6605, -0.5876, -0.0728), c),
        dot(vec3<f32>(-0.1246, 1.1329, -0.0083), c),
        dot(vec3<f32>(-0.0182, -0.1006, 1.1187), c));
}

fn max3(c: vec3<f32>) -> f32 {
    return max(max(c.r, c.g), c.b);
}

// Display light in nits, BT.709 primaries (channels may be negative until
// gamut_compress), of PQ (transfer 1) or HLG (2) R'G'B'.
fn hdr_nits(rgb: vec3<f32>, transfer: u32, wide: bool) -> vec3<f32> {
    var nits: vec3<f32>;
    if (transfer == 1u) {
        nits = pq_eotf(rgb);
    } else {
        // HLG's OOTF for a 1000-nit display, on the source's own luminance.
        let scene = max(hlg_inverse_oetf(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0))), vec3<f32>(0.0));
        let k = select(vec3<f32>(0.2126, 0.7152, 0.0722), vec3<f32>(0.2627, 0.6780, 0.0593), wide);
        nits = scene * pow(max(dot(k, scene), 1e-6), 0.2) * 1000.0;
    }
    if (wide) { nits = bt2020_to_709(nits); }
    return nits;
}
