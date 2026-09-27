// The fused "live suffix" of Tapetum's render graph: everything downstream of the bake boundary
// that must recompute every frame -- white balance -> exposure -> tone-curve LUT -> vibrance.
// Ported from spikes/glint/shaders/live_chain.wgsl (#16/ADR-0005, already measured at 0.326ms p95
// on the reference RTX 5080 at 4K, Vulkan) unchanged in kernel logic -- this is the same fused
// dispatch ADR-0005 validated the *approach* for; loaf's own contribution is chaining it after a
// real cache-key/graph model and measuring it as one stage of a larger pipeline, not re-deriving
// the kernel itself. See src/gpu.rs::run_live_suffix and tests/gpu_parity.rs.

struct Params {
    wb_r: f32,
    wb_g: f32,
    wb_b: f32,
    exposure_stops: f32,
    vibrance: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
}

@group(0) @binding(0) var<storage, read> input_pixels: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read_write> output_pixels: array<vec4<f32>>;
@group(0) @binding(2) var<uniform> params: Params;

fn tone_curve(x_in: f32) -> f32 {
    let xs = array<f32, 5>(0.0, 0.25, 0.5, 0.75, 1.0);
    let ys = array<f32, 5>(0.02, 0.22, 0.5, 0.80, 0.98);
    let x = clamp(x_in, 0.0, 1.0);
    for (var i: u32 = 0u; i < 4u; i = i + 1u) {
        if (x <= xs[i + 1u] || i == 3u) {
            let t = (x - xs[i]) / (xs[i + 1u] - xs[i]);
            return ys[i] + t * (ys[i + 1u] - ys[i]);
        }
    }
    return ys[4];
}

fn apply_vibrance(rgb_in: vec3<f32>, vibrance: f32) -> vec3<f32> {
    let mx = max(rgb_in.r, max(rgb_in.g, rgb_in.b));
    let mn = min(rgb_in.r, min(rgb_in.g, rgb_in.b));
    var sat = 0.0;
    if (mx > 0.0) {
        sat = (mx - mn) / mx;
    }
    let weight = vibrance * (1.0 - sat);
    let avg = (rgb_in.r + rgb_in.g + rgb_in.b) / 3.0;
    return clamp(vec3<f32>(avg) + (rgb_in - vec3<f32>(avg)) * (1.0 + weight), vec3<f32>(0.0), vec3<f32>(1.0));
}

@compute @workgroup_size(64)
fn live_suffix(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) num_wg: vec3<u32>) {
    let i = gid.x + gid.y * (num_wg.x * 64u);
    if (i >= arrayLength(&input_pixels)) {
        return;
    }
    let px = input_pixels[i];
    let exposure_mul = exp2(params.exposure_stops);
    let wb_gain = vec3<f32>(params.wb_r, params.wb_g, params.wb_b);
    var c = px.rgb * wb_gain * exposure_mul;
    c = vec3<f32>(tone_curve(c.r), tone_curve(c.g), tone_curve(c.b));
    c = apply_vibrance(c, params.vibrance);
    output_pixels[i] = vec4<f32>(c, px.a);
}
