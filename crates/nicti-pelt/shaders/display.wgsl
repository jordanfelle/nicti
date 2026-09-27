// Fullscreen-triangle blit from a Tapetum `FrameTexture` (linear ProPhoto RGB, Rgba16Float) to
// egui's own render-pass color target. Matches `nicti_tapetum::geometry::output_encode`'s CPU
// reference exactly: working-space -> linear sRGB via the ProPhoto->sRGB matrix, then the sRGB
// OETF -- except the OETF is skipped when the target itself is an `*Srgb` format, since the
// hardware already applies it on write in that case (applying it twice would double-gamma the
// image). `u.apply_oetf` is 0/1 rather than a `bool` -- WGSL uniform buffers don't guarantee a
// `bool`'s host-side layout, `u32` does.
//
// No `Sampler`: `textureLoad` reads the exact backing texel with no filtering, appropriate for a
// 1:1 present (a real zoom/pan resample already happened upstream, in Tapetum's own geometry/crop
// stage) -- same choice `pelt-egui`'s own `quad.wgsl` made for its `Rgba32Float` viewport texture.

struct Uniforms {
    // Row-major 3x3 linear ProPhoto RGB -> linear sRGB matrix (color::prophoto_to_srgb_linear_matrix),
    // laid out as three vec4 columns (matching nicti-tapetum::stages::LiveUniforms's own
    // vec4-per-column convention, to sidestep std140/WGSL's row-of-3 alignment padding).
    col0: vec4<f32>,
    col1: vec4<f32>,
    col2: vec4<f32>,
    apply_oetf: u32,
    _pad: vec3<u32>,
}

@group(0) @binding(0) var frame_tex: texture_2d<f32>;
@group(0) @binding(1) var<uniform> u: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VsOut {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let p = positions[vertex_index];
    var out: VsOut;
    out.pos = vec4<f32>(p, 0.0, 1.0);
    out.uv = vec2<f32>((p.x + 1.0) * 0.5, 1.0 - (p.y + 1.0) * 0.5);
    return out;
}

fn srgb_oetf(c: f32) -> f32 {
    let clamped = clamp(c, 0.0, 1.0);
    if (clamped <= 0.0031308) {
        return clamped * 12.92;
    }
    return 1.055 * pow(clamped, 1.0 / 2.4) - 0.055;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(frame_tex));
    let coord = vec2<i32>(in.uv * dims);
    let working_space = textureLoad(frame_tex, coord, 0);

    let m = mat3x3<f32>(u.col0.xyz, u.col1.xyz, u.col2.xyz);
    var srgb_linear = m * working_space.rgb;

    if (u.apply_oetf != 0u) {
        srgb_linear = vec3<f32>(
            srgb_oetf(srgb_linear.x),
            srgb_oetf(srgb_linear.y),
            srgb_oetf(srgb_linear.z),
        );
    }

    return vec4<f32>(srgb_linear, working_space.a);
}
