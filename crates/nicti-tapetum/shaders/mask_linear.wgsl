// Linear-gradient mask weight (#49): 1 at p0, ramping linearly to 0 at p1, constant across.
// Pixel-space endpoints; pixel centres sit at +0.5. CPU twin: mask::raster::linear_weight.

struct Uniforms {
    p: vec4<f32>,    // p0.xy, p1.xy
    dims: vec4<f32>, // width, height, 0, 0
}

@group(0) @binding(0) var out_tex: texture_storage_2d<r32float, write>;
@group(0) @binding(1) var<uniform> u: Uniforms;

@compute @workgroup_size(8, 8)
fn mask_linear(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.dims.x) || gid.y >= u32(u.dims.y)) {
        return;
    }
    let pos = vec2<f32>(f32(gid.x) + 0.5, f32(gid.y) + 0.5);
    let d = u.p.zw - u.p.xy;
    let len_sq = dot(d, d);
    var t = 0.0;
    if (len_sq > 1.1920929e-7) {
        t = dot(pos - u.p.xy, d) / len_sq;
    }
    let w = clamp(1.0 - clamp(t, 0.0, 1.0), 0.0, 1.0);
    textureStore(out_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(w, 0.0, 0.0, 1.0));
}
