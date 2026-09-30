// Guided filter step 5 (#49): bilinear-upsample the smoothed (a, b) to the mask extent and apply
// them to the full-resolution guide, so the alpha's edge follows the photo's real edges.

struct Uniforms {
    d: vec4<f32>, // out w, out h, low w, low h
}

@group(0) @binding(0) var guide_tex: texture_2d<f32>;
@group(0) @binding(1) var ab_tex: texture_2d<f32>;
@group(0) @binding(2) var out_tex: texture_storage_2d<r32float, write>;
@group(0) @binding(3) var<uniform> u: Uniforms;

fn ab_at(x: i32, y: i32) -> vec2<f32> {
    return textureLoad(ab_tex, vec2<i32>(clamp(x, 0, i32(u.d.z) - 1), clamp(y, 0, i32(u.d.w) - 1)), 0).xy;
}

@compute @workgroup_size(8, 8)
fn guided_apply(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.d.x) || gid.y >= u32(u.d.y)) {
        return;
    }
    let fx = (f32(gid.x) + 0.5) / u.d.x * u.d.z - 0.5;
    let fy = (f32(gid.y) + 0.5) / u.d.y * u.d.w - 0.5;
    let x0 = floor(fx);
    let y0 = floor(fy);
    let tx = fx - x0;
    let ty = fy - y0;
    let top = ab_at(i32(x0), i32(y0)) * (1.0 - tx) + ab_at(i32(x0) + 1, i32(y0)) * tx;
    let bottom = ab_at(i32(x0), i32(y0) + 1) * (1.0 - tx) + ab_at(i32(x0) + 1, i32(y0) + 1) * tx;
    let ab = top * (1.0 - ty) + bottom * ty;
    let px = vec2<i32>(i32(gid.x), i32(gid.y));
    let g = textureLoad(guide_tex, px, 0).r;
    textureStore(out_tex, px, vec4<f32>(clamp(ab.x * g + ab.y, 0.0, 1.0), 0.0, 0.0, 1.0));
}
