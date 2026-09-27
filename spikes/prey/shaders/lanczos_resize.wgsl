// Separable Lanczos3 resize over a linear-light RGBA (vec4<f32>, alpha unused/zero) storage
// buffer. Weights are precomputed on the CPU (gpu_resize.rs::compute_taps) into a per-output-line
// TapSet -- explicit (index, weight) pairs rather than a contiguous start+count range, since
// edge-clamped taps can collapse onto the same source index and aren't necessarily contiguous
// after clamping.

const MAX_TAPS: u32 = 96u;

struct TapSet {
    count: u32,
    idx: array<i32, 96>,
    weight: array<f32, 96>,
}

struct Dims {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
}

@group(0) @binding(0) var<storage, read> src_pixels: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read_write> intermediate_pixels: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read> taps_x: array<TapSet>;
@group(0) @binding(3) var<uniform> dims: Dims;

// Pass 1: resize horizontally. src_width x src_height -> dst_width x src_height.
@compute @workgroup_size(64)
fn resize_horizontal(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) num_wg: vec3<u32>) {
    let i = gid.x + gid.y * (num_wg.x * 64u);
    let total = dims.dst_width * dims.src_height;
    if (i >= total) {
        return;
    }
    let x_out = i % dims.dst_width;
    let y = i / dims.dst_width;

    let taps = taps_x[x_out];
    var sum = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    for (var j: u32 = 0u; j < taps.count; j = j + 1u) {
        let src_x = u32(taps.idx[j]);
        sum = sum + src_pixels[y * dims.src_width + src_x] * taps.weight[j];
    }
    intermediate_pixels[y * dims.dst_width + x_out] = sum;
}

@group(1) @binding(0) var<storage, read> intermediate_pixels_ro: array<vec4<f32>>;
@group(1) @binding(1) var<storage, read_write> dst_pixels: array<vec4<f32>>;
@group(1) @binding(2) var<storage, read> taps_y: array<TapSet>;
@group(1) @binding(3) var<uniform> dims2: Dims;

// Pass 2: resize vertically. dst_width x src_height -> dst_width x dst_height.
@compute @workgroup_size(64)
fn resize_vertical(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) num_wg: vec3<u32>) {
    let i = gid.x + gid.y * (num_wg.x * 64u);
    let total = dims2.dst_width * dims2.dst_height;
    if (i >= total) {
        return;
    }
    let x = i % dims2.dst_width;
    let y_out = i / dims2.dst_width;

    let taps = taps_y[y_out];
    var sum = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    for (var j: u32 = 0u; j < taps.count; j = j + 1u) {
        let src_y = u32(taps.idx[j]);
        sum = sum + intermediate_pixels_ro[src_y * dims2.dst_width + x] * taps.weight[j];
    }
    dst_pixels[y_out * dims2.dst_width + x] = sum;
}
