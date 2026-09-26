// WGSL twin of one iteration of compose.rs::compose's per-component fold (invert -> opacity ->
// combine-with-running-composite via op). Called once per `MaskComponent`, chaining
// `compose_out` back in as the next call's `running` -- matches the CPU reference's sequential
// fold exactly. `op`: 0 = Add, 1 = Subtract, 2 = Intersect (matches compose::Op's declared order).
// Checked against the CPU reference in tests/gpu_parity.rs within 1e-4 tolerance, skipping
// cleanly with no adapter.

struct Params {
    width: u32,
    height: u32,
    invert: u32,
    op: u32,
    opacity: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> running: array<f32>;
@group(0) @binding(2) var<storage, read> weight: array<f32>;
@group(0) @binding(3) var<storage, read_write> out_field: array<f32>;

@compute @workgroup_size(64, 1, 1)
fn compose_step(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) num_wg: vec3<u32>) {
    let i = gid.x + gid.y * num_wg.x * 64u;
    if (i >= params.width * params.height) {
        return;
    }
    var w = weight[i];
    if (params.invert != 0u) {
        w = 1.0 - w;
    }
    w = w * params.opacity;
    let r = running[i];
    if (params.op == 0u) {
        out_field[i] = min(r + w, 1.0);
    } else if (params.op == 1u) {
        out_field[i] = max(r - w, 0.0);
    } else {
        out_field[i] = r * w;
    }
}
