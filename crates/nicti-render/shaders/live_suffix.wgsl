// The fused live suffix (ADR-0044's "one fused live dispatch"): white balance + camera->working
// -space color (folded into one 3x3 on the CPU, see color.rs::camera_to_working_space_matrix),
// exposure, a simple tone curve, and vibrance -- exactly one dispatch regardless of how many of
// these params changed. Output stays in linear ProPhoto RGB (the working space); no display/
// export color management here (#42's scope, see color.rs's own doc comment on the deliberate
// simplification vs. the original design sketch: no HueSatMap/LookTable bindings are reserved).

struct Uniforms {
    // Row-major 3x3 camera-RGB -> working-space matrix, one column per vec4 (w unused, alignment
    // only) to avoid relying on WGSL's mat3x3 uniform-buffer layout matching Rust's.
    col0: vec4<f32>,
    col1: vec4<f32>,
    col2: vec4<f32>,
    exposure_mult: f32,
    contrast: f32,
    vibrance: f32,
    _pad: f32,
}

@group(0) @binding(0) var input_tex: texture_storage_2d<rgba16float, read>;
@group(0) @binding(1) var output_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> u: Uniforms;

fn apply_tone(rgb: vec3<f32>, contrast: f32) -> vec3<f32> {
    let c = max(rgb, vec3<f32>(0.0));
    let perceptual = pow(c, vec3<f32>(1.0 / 3.0));
    let adjusted = (perceptual - vec3<f32>(0.5)) * (1.0 + contrast) + vec3<f32>(0.5);
    let clamped = max(adjusted, vec3<f32>(0.0));
    return clamped * clamped * clamped;
}

fn apply_vibrance(rgb: vec3<f32>, vibrance: f32) -> vec3<f32> {
    let mx = max(rgb.r, max(rgb.g, rgb.b));
    let mn = min(rgb.r, min(rgb.g, rgb.b));
    var sat = 0.0;
    if (mx > 0.0) {
        sat = (mx - mn) / mx;
    }
    let boost = vibrance * (1.0 - sat);
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    return vec3<f32>(luma) + (rgb - vec3<f32>(luma)) * (1.0 + boost);
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(input_tex);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let px = textureLoad(input_tex, vec2<i32>(i32(gid.x), i32(gid.y)));
    let m = mat3x3<f32>(u.col0.xyz, u.col1.xyz, u.col2.xyz);
    var rgb = m * px.rgb;
    rgb = rgb * u.exposure_mult;
    rgb = apply_tone(rgb, u.contrast);
    rgb = apply_vibrance(rgb, u.vibrance);
    textureStore(output_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(rgb, px.a));
}
