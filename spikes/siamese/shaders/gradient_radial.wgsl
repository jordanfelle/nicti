// WGSL twin of geometry.rs's RadialGradient::weight. Checked against the CPU reference in
// tests/gpu_parity.rs within 1e-4 tolerance, skipping cleanly with no adapter. Same flat-scalar
// `Params` layout convention as gradient_linear.wgsl -- see its header comment.

struct Params {
    width: u32,
    height: u32,
    invert: u32,
    _pad: u32,
    center_x: f32,
    center_y: f32,
    radius_x: f32,
    radius_y: f32,
    angle: f32,
    feather: f32,
    _pad2: f32,
    _pad3: f32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read_write> out_field: array<f32>;

@compute @workgroup_size(64, 1, 1)
fn rasterize_radial_gradient(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) num_wg: vec3<u32>) {
    let i = gid.x + gid.y * num_wg.x * 64u;
    if (i >= params.width * params.height) {
        return;
    }
    let x = f32(i % params.width) + 0.5;
    let y = f32(i / params.width) + 0.5;
    let p = vec2<f32>(x, y);

    let rx = params.radius_x;
    let ry = params.radius_y;
    if (rx <= 0.0 || ry <= 0.0) {
        out_field[i] = select(0.0, 1.0, params.invert != 0u);
        return;
    }
    let d = p - vec2<f32>(params.center_x, params.center_y);
    let cos_a = cos(params.angle);
    let sin_a = sin(params.angle);
    let rot_x = d.x * cos_a + d.y * sin_a;
    let rot_y = -d.x * sin_a + d.y * cos_a;
    let normalized = sqrt((rot_x / rx) * (rot_x / rx) + (rot_y / ry) * (rot_y / ry));
    let mean_radius = (rx + ry) * 0.5;
    var feather_norm = 1e-6;
    if (mean_radius > 0.0) {
        feather_norm = max(params.feather / mean_radius, 1e-6);
    }
    var w = clamp(1.0 - (normalized - 1.0) / feather_norm, 0.0, 1.0);
    if (params.invert != 0u) {
        w = 1.0 - w;
    }
    out_field[i] = w;
}
