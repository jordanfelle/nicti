// GPU twin of src/refine.rs::box_filter -- the one primitive of the guided-filter mask refine
// this spike ports to GPU, proving the mechanism (not the whole guided-filter pipeline, which
// stays CPU-only here; see refine.rs's module doc for why). A naive O(width*height*(2r+1)^2)
// per-pixel loop, same complexity as the CPU reference -- fine at preview-mask resolution, not
// meant to be a fast production box filter (a real one would separate the two 1D passes).

struct Params {
    width: u32,
    height: u32,
    radius: u32,
    _pad: u32,
}

@group(0) @binding(0) var<storage, read> input_field: array<f32>;
@group(0) @binding(1) var<storage, read_write> output_field: array<f32>;
@group(0) @binding(2) var<uniform> params: Params;

@compute @workgroup_size(64)
fn box_filter(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) num_wg: vec3<u32>) {
    let i = gid.x + gid.y * (num_wg.x * 64u);
    let total = params.width * params.height;
    if (i >= total) {
        return;
    }
    let x = i32(i % params.width);
    let y = i32(i / params.width);
    let r = i32(params.radius);

    var sum: f32 = 0.0;
    var count: f32 = 0.0;
    for (var dy: i32 = -r; dy <= r; dy = dy + 1) {
        let yy = y + dy;
        if (yy < 0 || yy >= i32(params.height)) {
            continue;
        }
        for (var dx: i32 = -r; dx <= r; dx = dx + 1) {
            let xx = x + dx;
            if (xx < 0 || xx >= i32(params.width)) {
                continue;
            }
            sum = sum + input_field[u32(yy) * params.width + u32(xx)];
            count = count + 1.0;
        }
    }
    output_field[i] = select(0.0, sum / count, count > 0.0);
}
