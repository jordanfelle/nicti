// Crop/rotate/zoom/pan as a geometry-only sample pass -- #44's own ticket body: "Crop as geometry
// only: crop mode is a vertex transform over the already-rendered frame, nothing upstream
// re-renders while dragging." Reads the already-live-suffix-rendered frame from a storage buffer
// (source_width x source_height, laid out row-major) and writes a resampled output at
// out_width x out_height, via the affine transform in `Params` -- CPU reference and the exact
// affine convention are in src/geometry.rs (`source = matrix * output`).

struct Params {
    // Row-major 2x3: m0 = [m00, m01, m02], m1 = [m10, m11, m12].
    m0: vec3<f32>,
    m1: vec3<f32>,
    source_width: u32,
    source_height: u32,
    out_width: u32,
    out_height: u32,
}

@group(0) @binding(0) var<storage, read> source_pixels: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read_write> output_pixels: array<vec4<f32>>;
@group(0) @binding(2) var<uniform> params: Params;

fn sample_clamped(x: i32, y: i32) -> vec4<f32> {
    let xc = clamp(x, 0, i32(params.source_width) - 1);
    let yc = clamp(y, 0, i32(params.source_height) - 1);
    return source_pixels[u32(yc) * params.source_width + u32(xc)];
}

fn sample_bilinear(sx: f32, sy: f32) -> vec4<f32> {
    let x0f = floor(sx);
    let y0f = floor(sy);
    let tx = sx - x0f;
    let ty = sy - y0f;
    let x0 = i32(x0f);
    let y0 = i32(y0f);
    let p00 = sample_clamped(x0, y0);
    let p10 = sample_clamped(x0 + 1, y0);
    let p01 = sample_clamped(x0, y0 + 1);
    let p11 = sample_clamped(x0 + 1, y0 + 1);
    let top = mix(p00, p10, tx);
    let bottom = mix(p01, p11, tx);
    return mix(top, bottom, ty);
}

@compute @workgroup_size(64)
fn present_sample(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) num_wg: vec3<u32>) {
    let i = gid.x + gid.y * (num_wg.x * 64u);
    let total = params.out_width * params.out_height;
    if (i >= total) {
        return;
    }
    let ox = f32(i % params.out_width) + 0.5;
    let oy = f32(i / params.out_width) + 0.5;
    let sx = params.m0.x * ox + params.m0.y * oy + params.m0.z;
    let sy = params.m1.x * ox + params.m1.y * oy + params.m1.z;
    output_pixels[i] = sample_bilinear(sx - 0.5, sy - 0.5);
}
