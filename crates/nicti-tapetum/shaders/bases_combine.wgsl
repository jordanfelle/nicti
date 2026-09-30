// Packs the clarity/texture bases (#49): from the baked perceptual luma `g` and its two
// edge-preserving (self-guided) smoothings, the fine band g - base_fine and the mid band
// base_fine - base_coarse, plus g itself, into one bilinear-sampled RGBA16F texture.
// CPU twin: mask::bases::bands_cpu.

struct Uniforms {
    d: vec4<f32>, // w, h, 0, 0
}

@group(0) @binding(0) var g_tex: texture_2d<f32>;
@group(0) @binding(1) var fine_tex: texture_2d<f32>;
@group(0) @binding(2) var coarse_tex: texture_2d<f32>;
@group(0) @binding(3) var out_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(4) var<uniform> u: Uniforms;

@compute @workgroup_size(8, 8)
fn bases_combine(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= u32(u.d.x) || gid.y >= u32(u.d.y)) {
        return;
    }
    let px = vec2<i32>(i32(gid.x), i32(gid.y));
    let g = textureLoad(g_tex, px, 0).r;
    let f = textureLoad(fine_tex, px, 0).r;
    let c = textureLoad(coarse_tex, px, 0).r;
    textureStore(out_tex, px, vec4<f32>(g - f, f - c, g, 0.0));
}
