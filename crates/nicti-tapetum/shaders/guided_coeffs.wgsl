// Guided filter step 3 (#49): from the box-filtered statistics, the per-window linear fit
// alpha ~= a * I + b, with `eps` regularizing flat regions.

struct Uniforms {
    d: vec4<f32>, // w, h, eps, 0
}

@group(0) @binding(0) var means_tex: texture_2d<f32>;
@group(0) @binding(1) var out_tex: texture_storage_2d<rgba32float, write>;
@group(0) @binding(2) var<uniform> u: Uniforms;

@compute @workgroup_size(8, 8)
fn guided_coeffs(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.d.x) || gid.y >= u32(u.d.y)) {
        return;
    }
    let px = vec2<i32>(i32(gid.x), i32(gid.y));
    let m = textureLoad(means_tex, px, 0);
    let variance = m.z - m.x * m.x;
    let covariance = m.w - m.x * m.y;
    let a = covariance / (variance + u.d.z);
    let b = m.y - a * m.x;
    textureStore(out_tex, px, vec4<f32>(a, b, 0.0, 0.0));
}
