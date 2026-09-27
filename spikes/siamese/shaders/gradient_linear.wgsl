// WGSL twin of geometry.rs's LinearGradient::weight. Checked against the CPU reference in
// tests/gpu_parity.rs within 1e-4 tolerance (matches groom/calico's own parity-test convention),
// skipping cleanly with no adapter. Params is a flat sequence of 4-byte scalars (no vec2/vec4
// fields) so its WGSL uniform-buffer layout matches a `#[repr(C)]` Rust struct byte-for-byte with
// no manual padding math -- every field is 4-byte aligned and offsets fall out sequentially.

struct Params {
    width: u32,
    height: u32,
    invert: u32,
    _pad: u32,
    p0x: f32,
    p0y: f32,
    p1x: f32,
    p1y: f32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read_write> out_field: array<f32>;

@compute @workgroup_size(64, 1, 1)
fn rasterize_linear_gradient(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) num_wg: vec3<u32>) {
    let i = gid.x + gid.y * num_wg.x * 64u;
    if (i >= params.width * params.height) {
        return;
    }
    let x = f32(i % params.width) + 0.5;
    let y = f32(i / params.width) + 0.5;
    let p = vec2<f32>(x, y);
    let p0 = vec2<f32>(params.p0x, params.p0y);
    let p1 = vec2<f32>(params.p1x, params.p1y);

    let d = p1 - p0;
    let len_sq = dot(d, d);
    var t: f32 = 0.0;
    if (len_sq > 1e-8) {
        t = dot(p - p0, d) / len_sq;
    }
    var w = clamp(1.0 - clamp(t, 0.0, 1.0), 0.0, 1.0);
    if (params.invert != 0u) {
        w = 1.0 - w;
    }
    out_field[i] = w;
}
