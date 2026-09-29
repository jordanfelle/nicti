// Fullscreen-triangle blit from a Tapetum `FrameTexture` (linear ProPhoto RGB, Rgba16Float) to
// egui's own render-pass color target. Two display modes (ADR-0042, `nicti_calico::transform::
// DisplayTransform`): mode 0 is the exact matrix + transfer-function path (matches
// `nicti_tapetum::geometry::output_encode`'s CPU reference for sRGB); mode 1 is a baked 3D LUT for
// a real monitor ICC profile and/or soft-proofing, with an optional gamut-warning tint. Either
// way the result is display-*encoded*; when the target itself is an `*Srgb` format the hardware
// applies the sRGB OETF on write, so the shader undoes it first (`target_srgb`) rather than
// double-gamma the image. The flags are 0/1 `u32`s rather than `bool`s -- WGSL uniform buffers
// don't guarantee a `bool`'s host-side layout, `u32` does.
//
// #31 phase 3: `view_scale`/`view_offset` map screen UV to texture UV (both default `(1,1)`/
// `(0,0)`, reproducing the old always-1:1-stretch behavior exactly for the Develop tab, which
// never sets them to anything else). The Loupe view sets them for aspect-correct "Fit" and 1:1
// "100%" zoom (`viewport.rs`'s own doc comment has the derivation). A real sampler + bilinear
// filtering replaces the old `textureLoad` (no resample needed at exact 1:1 pre-#31, since
// `nicti_pelt`'s only caller was the Develop tab's own always-stretched view) -- bilinear at an
// exact 1:1 scale/offset samples precisely on texel centers, so it's visually identical to the
// old nearest-neighbor read there, and is the correct behavior for a scaled "Fit" view.
// Out-of-`[0,1]` texture UV (the letterboxed bars in "Fit" mode when the rect and image aspect
// ratios don't match) renders a fixed background color rather than clamping/repeating the edge
// texel.

struct Uniforms {
    // Row-major 3x3 linear ProPhoto RGB -> linear sRGB matrix (color::prophoto_to_srgb_linear_matrix),
    // laid out as three vec4 columns (matching nicti-tapetum::stages::LiveUniforms's own
    // vec4-per-column convention, to sidestep std140/WGSL's row-of-3 alignment padding).
    col0: vec4<f32>,
    col1: vec4<f32>,
    col2: vec4<f32>,
    // Screen-UV-to-texture-UV scale/offset (see this file's own header comment) -- vec2 has
    // align/size 8 in WGSL's uniform-address-space layout rules, placed before the scalar tail so
    // nothing needs an explicit alignment-bump pad the way a trailing vec3<u32> would (see below).
    view_scale: vec2<f32>,
    view_offset: vec2<f32>,
    // 1 when the render target is an `*Srgb` format: hardware then applies the sRGB OETF on
    // write, so the shader hands it linear values (`srgb_eotf` of the display-encoded result).
    target_srgb: u32,
    // 0 = exact matrix + transfer function (`col0..2`, `trc`), 1 = baked 3D LUT (ADR-0042).
    mode: u32,
    // Mode 0 transfer function: 0 = sRGB curve (sRGB, Display P3), 1 = Adobe RGB gamma 563/256.
    trc: u32,
    // 1 = tint pixels the LUT flags as outside the proof space's gamut (alpha channel).
    gamut_warn: u32,
    // Four scalar u32s, not a `vec4<u32>`/`vec3<u32>` pad -- a vecN field would itself need align
    // 16 in WGSL's uniform-address-space layout rules, pushing the struct's total size past what
    // the host-side `DisplayUniforms` (plain `u32`s, no vecN alignment) actually allocates -- a
    // real min-binding-size mismatch caught in review once already for this same struct. Matches
    // `nicti-tapetum::stages::LiveUniforms`'s own scalar-tail pattern for the same reason.
}

@group(0) @binding(0) var frame_tex: texture_2d<f32>;
@group(0) @binding(1) var frame_sampler: sampler;
@group(0) @binding(2) var<uniform> u: Uniforms;
// ADR-0042 display/proof LUT: 33^3, red = x, green = y, blue = z; rgb = display-encoded output,
// a = out-of-proof-gamut flag. A 1^3 dummy is bound in mode 0.
@group(0) @binding(3) var lut_tex: texture_3d<f32>;
@group(0) @binding(4) var lut_sampler: sampler;

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

fn srgb_eotf(c: f32) -> f32 {
    let clamped = clamp(c, 0.0, 1.0);
    if (clamped <= 0.04045) {
        return clamped / 12.92;
    }
    return pow((clamped + 0.055) / 1.055, 2.4);
}

// Adobe RGB (1998)'s pure power-law exponent, 563/256.
const ADOBE_RGB_GAMMA: f32 = 2.19921875;
// ProPhoto's transfer exponent -- the LUT's input shaper (calico `PROPHOTO_GAMMA`).
const PROPHOTO_GAMMA: f32 = 1.8;
// Gamut-warning overlay color (encoded, display space).
const GAMUT_WARN: vec3<f32> = vec3<f32>(0.9, 0.1, 0.55);

fn encode_trc(c: f32) -> f32 {
    if (u.trc == 1u) {
        return pow(clamp(c, 0.0, 1.0), 1.0 / ADOBE_RGB_GAMMA);
    }
    return srgb_oetf(c);
}

// Dark neutral gray, distinguishable from a genuinely black photo's own shadows -- the letterbox
// background in "Fit" mode when the rect and image aspect ratios don't match.
const LETTERBOX_BG: vec4<f32> = vec4<f32>(0.05, 0.05, 0.05, 1.0);

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let tex_uv = (in.uv - vec2<f32>(0.5, 0.5)) * u.view_scale + vec2<f32>(0.5, 0.5) + u.view_offset;
    if (tex_uv.x < 0.0 || tex_uv.x > 1.0 || tex_uv.y < 0.0 || tex_uv.y > 1.0) {
        return LETTERBOX_BG;
    }
    let working_space = textureSample(frame_tex, frame_sampler, tex_uv);

    var encoded: vec3<f32>;
    if (u.mode == 1u) {
        // Shaper (linear ProPhoto -> gamma 1.8), then trilinear LUT sample. Texel-centered
        // addressing: texel i sits at (i + 0.5) / N, so remap [0,1] onto that range.
        let n = f32(textureDimensions(lut_tex).x);
        let shaped = pow(clamp(working_space.rgb, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / PROPHOTO_GAMMA));
        let coord = shaped * ((n - 1.0) / n) + vec3<f32>(0.5 / n);
        let texel = textureSampleLevel(lut_tex, lut_sampler, coord, 0.0);
        encoded = texel.rgb;
        if (u.gamut_warn != 0u && texel.a > 0.5) {
            encoded = GAMUT_WARN;
        }
    } else {
        let m = mat3x3<f32>(u.col0.xyz, u.col1.xyz, u.col2.xyz);
        let linear_out = m * working_space.rgb;
        encoded = vec3<f32>(
            encode_trc(linear_out.x),
            encode_trc(linear_out.y),
            encode_trc(linear_out.z),
        );
    }

    if (u.target_srgb != 0u) {
        encoded = vec3<f32>(srgb_eotf(encoded.x), srgb_eotf(encoded.y), srgb_eotf(encoded.z));
    }
    return vec4<f32>(encoded, working_space.a);
}
