// Combines the per-pixel live-suffix output with its own NR-radius and sharpen-radius blurs into
// #46's Noise Reduction + Sharpening result -- mirrors `detail.rs::apply_detail_rgb` exactly (see
// its own doc comment for the luma/chroma split this implements).

struct Uniforms {
    // luminance, color, detail, unused.
    nr: vec4<f32>,
    // amount, unused (radius_px only matters for building the blur weights, not here), detail,
    // unused.
    sharpen: vec4<f32>,
}

@group(0) @binding(0) var original_tex: texture_storage_2d<rgba16float, read>;
@group(0) @binding(1) var nr_blurred_tex: texture_storage_2d<rgba16float, read>;
@group(0) @binding(2) var sharpen_blurred_tex: texture_storage_2d<rgba16float, read>;
@group(0) @binding(3) var output_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(4) var<uniform> u: Uniforms;

const LUMA_WEIGHTS: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);
const EDGE_SCALE: f32 = 0.08;

fn edge_weight(original: f32, fine_blur: f32) -> f32 {
    let diff = abs(original - fine_blur);
    return clamp(1.0 - diff / max(EDGE_SCALE, 1e-4), 0.0, 1.0);
}

// Mirrors detail.rs::apply_detail exactly (scalar, applied to luma only).
fn apply_detail_scalar(
    original: f32,
    blurred_nr: f32,
    blurred_sharpen: f32,
    weight: f32,
    nr_luminance: f32,
    nr_detail: f32,
    sharpen_amount: f32,
    sharpen_detail: f32,
) -> f32 {
    let nr_blend = clamp(nr_luminance * (1.0 - nr_detail * (1.0 - weight)), 0.0, 1.0);
    let denoised = original + (blurred_nr - original) * nr_blend;

    let sharpen_gate = clamp(sharpen_detail * (1.0 - weight) + (1.0 - sharpen_detail), 0.0, 1.0);
    let unsharp = denoised - blurred_sharpen;
    return denoised + sharpen_amount * unsharp * sharpen_gate;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(original_tex);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let coord = vec2<i32>(i32(gid.x), i32(gid.y));
    let original = textureLoad(original_tex, coord);
    let nr_blurred = textureLoad(nr_blurred_tex, coord);
    let sharpen_blurred = textureLoad(sharpen_blurred_tex, coord);

    let orig_luma = dot(original.rgb, LUMA_WEIGHTS);
    let nr_luma = dot(nr_blurred.rgb, LUMA_WEIGHTS);
    let sharpen_luma = dot(sharpen_blurred.rgb, LUMA_WEIGHTS);
    let weight = edge_weight(orig_luma, nr_luma);
    let new_luma = apply_detail_scalar(
        orig_luma, nr_luma, sharpen_luma, weight,
        u.nr.x, u.nr.z, u.sharpen.x, u.sharpen.z,
    );

    let color_amount = clamp(u.nr.y, 0.0, 1.0);
    let chroma_orig = original.rgb - vec3<f32>(orig_luma);
    let chroma_blurred = nr_blurred.rgb - vec3<f32>(nr_luma);
    let new_chroma = chroma_orig + (chroma_blurred - chroma_orig) * color_amount;

    let out_rgb = vec3<f32>(new_luma) + new_chroma;
    textureStore(output_tex, coord, vec4<f32>(out_rgb, original.a));
}
