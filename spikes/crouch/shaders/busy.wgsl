// A tunable-duration compute kernel: each invocation does `params.iterations` cheap FMA-style
// passes over its own element, so wall-clock dispatch cost scales with `iterations` in a way a
// contention bench can pick a target duration for (e.g. one SCUNet-tile-equivalent chunk).
// Not a stand-in for any real render stage -- ADR-0044's own `live_suffix`/`present_sample`
// kernels already measure the real foreground cost; this is deliberately synthetic so its cost is
// exactly controllable.

struct Params {
    iterations: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read_write> data: array<f32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x;
    if (idx >= arrayLength(&data)) {
        return;
    }
    var v = data[idx];
    for (var i: u32 = 0u; i < params.iterations; i = i + 1u) {
        v = fma(v, 1.0000001, 0.0000001);
    }
    data[idx] = v;
}
