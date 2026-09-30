// Dehaze step 2 (#49): one direction of the separable min filter that turns the per-pixel
// candidates into the dark channel. On the vertical pass (`dir` = 1) the result is also converted
// to a transmission estimate, t = clamp(1 - omega * dark, 0, 1), ready for the guided refine.

struct Uniforms {
    d: vec4<f32>, // w, h, radius, dir
    e: vec4<f32>, // omega, 0, 0, 0
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var out_tex: texture_storage_2d<r32float, write>;
@group(0) @binding(2) var<uniform> u: Uniforms;

@compute @workgroup_size(8, 8)
fn dehaze_minpass(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.d.x) || gid.y >= u32(u.d.y)) {
        return;
    }
    let r = i32(u.d.z);
    let x = i32(gid.x);
    let y = i32(gid.y);
    var best = 1.0e30;
    if (u.d.w < 0.5) {
        for (var i = max(x - r, 0); i <= min(x + r, i32(u.d.x) - 1); i = i + 1) {
            best = min(best, textureLoad(src_tex, vec2<i32>(i, y), 0).r);
        }
        textureStore(out_tex, vec2<i32>(x, y), vec4<f32>(best, 0.0, 0.0, 1.0));
    } else {
        for (var i = max(y - r, 0); i <= min(y + r, i32(u.d.y) - 1); i = i + 1) {
            best = min(best, textureLoad(src_tex, vec2<i32>(x, i), 0).r);
        }
        let t = clamp(1.0 - u.e.x * best, 0.0, 1.0);
        textureStore(out_tex, vec2<i32>(x, y), vec4<f32>(t, 0.0, 0.0, 1.0));
    }
}
