// One direction of a separable box filter over an RGBA32F statistics texture (#49). Averages over
// the valid (in-bounds) window only, so the border isn't darkened. `dir` 0 = horizontal, 1 = vertical.

struct Uniforms {
    d: vec4<f32>, // w, h, radius, dir
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var out_tex: texture_storage_2d<rgba32float, write>;
@group(0) @binding(2) var<uniform> u: Uniforms;

@compute @workgroup_size(8, 8)
fn guided_box(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.d.x) || gid.y >= u32(u.d.y)) {
        return;
    }
    let r = i32(u.d.z);
    let x = i32(gid.x);
    let y = i32(gid.y);
    var sum = vec4<f32>(0.0);
    var count = 0.0;
    if (u.d.w < 0.5) {
        let lo = max(x - r, 0);
        let hi = min(x + r, i32(u.d.x) - 1);
        for (var i = lo; i <= hi; i = i + 1) {
            sum = sum + textureLoad(src_tex, vec2<i32>(i, y), 0);
            count = count + 1.0;
        }
    } else {
        let lo = max(y - r, 0);
        let hi = min(y + r, i32(u.d.y) - 1);
        for (var i = lo; i <= hi; i = i + 1) {
            sum = sum + textureLoad(src_tex, vec2<i32>(x, i), 0);
            count = count + 1.0;
        }
    }
    textureStore(out_tex, vec2<i32>(x, y), sum / count);
}
