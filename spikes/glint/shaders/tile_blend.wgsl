// Feathered linear blend of two overlapping tiles along a seam -- the render-side half of tiled
// AI inference reconstruction. Mirrors src/cpu_reference.rs::tile_blend_pixel exactly.

struct Params {
    width: u32,
    seam_start: u32,
    seam_width: u32,
    _pad: u32,
}

@group(0) @binding(0) var<storage, read> tile_a: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read> tile_b: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read_write> output_pixels: array<vec4<f32>>;
@group(0) @binding(3) var<uniform> params: Params;

@compute @workgroup_size(64)
fn tile_blend(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) num_wg: vec3<u32>) {
    // See live_chain.wgsl's identical comment: 2D dispatch grid, flat index recovered here.
    let i = gid.x + gid.y * (num_wg.x * 64u);
    if (i >= arrayLength(&tile_a)) {
        return;
    }
    let col = i % params.width;
    var t = 0.0;
    if (params.seam_width > 0u) {
        let rel = f32(col) - f32(params.seam_start);
        t = clamp(rel / f32(params.seam_width), 0.0, 1.0);
    } else if (col >= params.seam_start) {
        t = 1.0;
    }
    let a = tile_a[i];
    let b = tile_b[i];
    output_pixels[i] = a + (b - a) * t;
}
