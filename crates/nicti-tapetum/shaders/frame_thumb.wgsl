// A small RGBA32F thumbnail of the baked frame (#49), for estimating the dehaze airlight on the
// CPU. Each thumbnail pixel averages an evenly strided grid (up to 8x8) of source texels over its
// footprint, so a single hot pixel can't dominate the estimate.

struct Uniforms {
    d: vec4<f32>, // frame w, frame h, thumb w, thumb h
}

@group(0) @binding(0) var frame_tex: texture_2d<f32>;
@group(0) @binding(1) var out_tex: texture_storage_2d<rgba32float, write>;
@group(0) @binding(2) var<uniform> u: Uniforms;

@compute @workgroup_size(8, 8)
fn frame_thumb(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.d.z) || gid.y >= u32(u.d.w)) {
        return;
    }
    let sx = u.d.x / u.d.z;
    let sy = u.d.y / u.d.w;
    let nx = min(u32(ceil(sx)), 8u);
    let ny = min(u32(ceil(sy)), 8u);
    var sum = vec3<f32>(0.0);
    for (var j = 0u; j < ny; j = j + 1u) {
        for (var i = 0u; i < nx; i = i + 1u) {
            let fx = (f32(gid.x) + (f32(i) + 0.5) / f32(nx)) * sx;
            let fy = (f32(gid.y) + (f32(j) + 0.5) / f32(ny)) * sy;
            let p = vec2<i32>(
                clamp(i32(fx), 0, i32(u.d.x) - 1),
                clamp(i32(fy), 0, i32(u.d.y) - 1),
            );
            sum = sum + textureLoad(frame_tex, p, 0).rgb;
        }
    }
    let avg = sum / f32(nx * ny);
    textureStore(out_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(avg, 1.0));
}
