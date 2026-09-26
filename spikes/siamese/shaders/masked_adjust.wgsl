// Masked-adjust apply: a local-adjustment exposure delta scaled by mask weight, in linear light --
// proves docs/adr/0025-masking.md's "local adjustments stay live in the shader, only AI alpha is
// baked" proposal. GpuPixel mirrors calico's own `rgb: vec3<f32>, _pad: f32` shape (see
// `spikes/calico/src/gpu.rs`'s `GpuPixel`) so both spikes' Rust-side struct stays the same
// bytemuck-friendly layout. Checked against the CPU reference in tests/gpu_parity.rs within 1e-4
// tolerance, skipping cleanly with no adapter.

struct GpuPixel {
    rgb: vec3<f32>,
    _pad: f32,
}

struct Params {
    width: u32,
    height: u32,
    exposure_ev: f32,
    _pad: f32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> in_pixels: array<GpuPixel>;
@group(0) @binding(2) var<storage, read> mask: array<f32>;
@group(0) @binding(3) var<storage, read_write> out_pixels: array<GpuPixel>;

@compute @workgroup_size(64, 1, 1)
fn masked_adjust_apply(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) num_wg: vec3<u32>) {
    let i = gid.x + gid.y * num_wg.x * 64u;
    if (i >= params.width * params.height) {
        return;
    }
    let m = mask[i];
    let scale = exp2(params.exposure_ev * m);
    var out: GpuPixel;
    out.rgb = in_pixels[i].rgb * scale;
    out._pad = 0.0;
    out_pixels[i] = out;
}
