// One Jacobi sweep of the discrete Poisson solve (Perez et al. 2003 "seamless cloning").
// Mirrors src/cpu_reference.rs::poisson_jacobi_step exactly -- keep the two in sync.
//
// `mask[i] == 0u` pixels are fixed boundary conditions (copy through unchanged); `mask[i] == 1u`
// pixels are the unknown interior being solved for, updated from their 4-neighbors' current
// values plus the guidance field's local gradient. The host issues one dispatch per iteration,
// ping-ponging `in_buf`/`out_buf` between two buffers rather than looping in-shader, so each
// dispatch does a fixed, single Jacobi step -- see gpu.rs::run_poisson_jacobi.
//
// The solve runs over RGB only; alpha (`.w`) always passes through from `in_buf[i]` unchanged,
// for both boundary and interior pixels -- opacity isn't a color channel and shouldn't be
// gradient-domain-blended, matching cpu_reference.rs::poisson_jacobi_step.

struct Params {
    width: u32,
    height: u32,
    _pad0: u32,
    _pad1: u32,
}

@group(0) @binding(0) var<storage, read> in_buf: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read_write> out_buf: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read> guidance: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> mask: array<u32>;
@group(0) @binding(4) var<uniform> params: Params;

@compute @workgroup_size(64)
fn poisson_jacobi(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) num_wg: vec3<u32>) {
    // 2D dispatch grid, flat index recovered here -- see gpu.rs::workgroup_grid / ADR-0005's
    // dispatch-dimensioning note (glint's tile_blend.wgsl carries the identical comment).
    let i = gid.x + gid.y * (num_wg.x * 64u);
    let total = params.width * params.height;
    if (i >= total) {
        return;
    }
    if (mask[i] == 0u) {
        out_buf[i] = in_buf[i];
        return;
    }

    let x = i % params.width;
    let y = i / params.width;
    var sum_f = vec3<f32>(0.0);
    var sum_g = vec3<f32>(0.0);
    var n = 0.0;

    if (x > 0u) {
        let j = i - 1u;
        sum_f += in_buf[j].xyz;
        sum_g += guidance[i].xyz - guidance[j].xyz;
        n += 1.0;
    }
    if (x + 1u < params.width) {
        let j = i + 1u;
        sum_f += in_buf[j].xyz;
        sum_g += guidance[i].xyz - guidance[j].xyz;
        n += 1.0;
    }
    if (y > 0u) {
        let j = i - params.width;
        sum_f += in_buf[j].xyz;
        sum_g += guidance[i].xyz - guidance[j].xyz;
        n += 1.0;
    }
    if (y + 1u < params.height) {
        let j = i + params.width;
        sum_f += in_buf[j].xyz;
        sum_g += guidance[i].xyz - guidance[j].xyz;
        n += 1.0;
    }

    // Alpha always passes through from `in_buf[i]` unchanged -- see this file's header comment.
    let alpha = in_buf[i].w;
    if (n > 0.0) {
        out_buf[i] = vec4<f32>((sum_f + sum_g) / n, alpha);
    } else {
        out_buf[i] = in_buf[i];
    }
}
