// Dehaze step 1 (#49): the per-pixel dark-channel candidate, min_c(rgb_c / A_c), taken per source
// tap and bilinearly combined at the bases extent (like mask_range). CPU twin:
// mask::bases::transmission_cpu.

struct Uniforms {
    a: vec4<f32>, // airlight rgb, 0
    d: vec4<f32>, // frame w, frame h, out w, out h
}

@group(0) @binding(0) var frame_tex: texture_2d<f32>;
@group(0) @binding(1) var out_tex: texture_storage_2d<r32float, write>;
@group(0) @binding(2) var<uniform> u: Uniforms;

fn candidate(px: vec2<i32>) -> f32 {
    let c = max(textureLoad(frame_tex, px, 0).rgb, vec3<f32>(0.0));
    let a = max(u.a.xyz, vec3<f32>(1e-4));
    let n = c / a;
    return min(n.x, min(n.y, n.z));
}

@compute @workgroup_size(8, 8)
fn dehaze_min0(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.d.z) || gid.y >= u32(u.d.w)) {
        return;
    }
    let fx = (f32(gid.x) + 0.5) / u.d.z * u.d.x - 0.5;
    let fy = (f32(gid.y) + 0.5) / u.d.w * u.d.y - 0.5;
    let x0 = floor(fx);
    let y0 = floor(fy);
    let tx = fx - x0;
    let ty = fy - y0;
    let maxx = i32(u.d.x) - 1;
    let maxy = i32(u.d.y) - 1;
    let ix0 = clamp(i32(x0), 0, maxx);
    let ix1 = clamp(i32(x0) + 1, 0, maxx);
    let iy0 = clamp(i32(y0), 0, maxy);
    let iy1 = clamp(i32(y0) + 1, 0, maxy);
    let top = candidate(vec2<i32>(ix0, iy0)) * (1.0 - tx) + candidate(vec2<i32>(ix1, iy0)) * tx;
    let bottom = candidate(vec2<i32>(ix0, iy1)) * (1.0 - tx) + candidate(vec2<i32>(ix1, iy1)) * tx;
    let v = top * (1.0 - ty) + bottom * ty;
    textureStore(out_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(v, 0.0, 0.0, 1.0));
}
