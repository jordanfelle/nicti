// WGSL twin of geometry.rs's Geometry::Brush rasterizer: a flat, stroke-ordered dab list folded
// sequentially per pixel exactly like the CPU reference's "for stroke in strokes { max-blend dabs,
// then max (add) or subtract (erase) into the running field }" loop -- dabs must arrive
// pre-sorted by stroke_id (ascending), which the CPU caller (gpu.rs::run_rasterize_brush)
// guarantees by construction. Checked against the CPU reference in tests/gpu_parity.rs within
// 1e-4 tolerance, skipping cleanly with no adapter.

struct Dims {
    width: u32,
    height: u32,
    _pad0: u32,
    _pad1: u32,
}

struct GpuDab {
    center_x: f32,
    center_y: f32,
    radius: f32,
    feather: f32,
    flow: f32,
    stroke_id: f32,
    erase: f32,
    _pad: f32,
}

@group(0) @binding(0) var<uniform> dims: Dims;
@group(0) @binding(1) var<storage, read> dabs: array<GpuDab>;
@group(0) @binding(2) var<storage, read_write> out_field: array<f32>;

fn dab_weight(d: GpuDab, p: vec2<f32>) -> f32 {
    let dist = distance(p, vec2<f32>(d.center_x, d.center_y));
    if (dist >= d.radius) {
        return 0.0;
    }
    let inner = max(d.radius - d.feather, 0.0);
    var w = 1.0;
    if (dist > inner) {
        w = 1.0 - (dist - inner) / max(d.radius - inner, 1e-6);
    }
    return clamp(w * d.flow, 0.0, 1.0);
}

@compute @workgroup_size(64, 1, 1)
fn rasterize_brush(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) num_wg: vec3<u32>) {
    let i = gid.x + gid.y * num_wg.x * 64u;
    if (i >= dims.width * dims.height) {
        return;
    }
    let x = f32(i % dims.width) + 0.5;
    let y = f32(i / dims.width) + 0.5;
    let p = vec2<f32>(x, y);

    var acc: f32 = 0.0;
    var current_id: f32 = -1.0;
    var current_erase: f32 = 0.0;
    var current_max: f32 = 0.0;
    let n = arrayLength(&dabs);
    for (var k = 0u; k < n; k = k + 1u) {
        let d = dabs[k];
        if (d.stroke_id != current_id) {
            if (current_id >= 0.0) {
                if (current_erase > 0.5) {
                    acc = max(acc - current_max, 0.0);
                } else {
                    acc = max(acc, current_max);
                }
            }
            current_id = d.stroke_id;
            current_erase = d.erase;
            current_max = 0.0;
        }
        current_max = max(current_max, dab_weight(d, p));
    }
    if (current_id >= 0.0) {
        if (current_erase > 0.5) {
            acc = max(acc - current_max, 0.0);
        } else {
            acc = max(acc, current_max);
        }
    }
    out_field[i] = acc;
}
