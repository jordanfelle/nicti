// Guided filter step 1 (#49): per low-res pixel, the statistics inputs (I, p, I*I, I*p), where I is
// the full-res guide luminance sampled down to the alpha's resolution and p is the alpha itself.

struct Uniforms {
    d: vec4<f32>, // guide w, guide h, low w, low h
}

@group(0) @binding(0) var guide_tex: texture_2d<f32>;
@group(0) @binding(1) var alpha_tex: texture_2d<f32>;
@group(0) @binding(2) var out_tex: texture_storage_2d<rgba32float, write>;
@group(0) @binding(3) var<uniform> u: Uniforms;

fn guide_at(x: i32, y: i32) -> f32 {
    return textureLoad(guide_tex, vec2<i32>(clamp(x, 0, i32(u.d.x) - 1), clamp(y, 0, i32(u.d.y) - 1)), 0).r;
}

@compute @workgroup_size(8, 8)
fn guided_prep(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.d.z) || gid.y >= u32(u.d.w)) {
        return;
    }
    let fx = (f32(gid.x) + 0.5) / u.d.z * u.d.x - 0.5;
    let fy = (f32(gid.y) + 0.5) / u.d.w * u.d.y - 0.5;
    let x0 = floor(fx);
    let y0 = floor(fy);
    let tx = fx - x0;
    let ty = fy - y0;
    let top = guide_at(i32(x0), i32(y0)) * (1.0 - tx) + guide_at(i32(x0) + 1, i32(y0)) * tx;
    let bottom = guide_at(i32(x0), i32(y0) + 1) * (1.0 - tx) + guide_at(i32(x0) + 1, i32(y0) + 1) * tx;
    let i = top * (1.0 - ty) + bottom * ty;
    let px = vec2<i32>(i32(gid.x), i32(gid.y));
    let p = textureLoad(alpha_tex, px, 0).r;
    textureStore(out_tex, px, vec4<f32>(i, p, i * i, i * p));
}
