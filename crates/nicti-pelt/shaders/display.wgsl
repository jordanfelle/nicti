// Fullscreen-triangle blit from a Tapetum `FrameTexture` (linear ProPhoto RGB, Rgba16Float) to
// egui's own render-pass color target, color-managed (ADR-0042, `nicti_calico::transform::
// DisplayTransform`). Two stages: an optional analytic soft-proof (clip in the proof space's
// linear RGB, exact out-of-gamut flag), then the display -- mode 0: exact matrix + built-in
// transfer function (within one 8-bit code of `nicti_tapetum::geometry::output_encode`'s CPU
// reference for sRGB, not bit-identical); mode 2: matrix + per-channel encode table for a
// matrix/TRC monitor profile; mode 1: baked 3D LUT, only for a LUT-based monitor profile. An
// optional gamut-warning tint follows. Either way the result is display-*encoded*; when the
// target itself is an `*Srgb` format the hardware applies the sRGB OETF on write, so the shader
// undoes it first (`target_srgb`) rather than double-gamma the image. The flags are 0/1 `u32`s
// rather than `bool`s -- WGSL uniform buffers don't guarantee a `bool`'s host-side layout, `u32`
// does.
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
    // Soft proof (ADR-0042): linear ProPhoto -> proof-space linear RGB (`pcol*`) and back
    // (`qcol*`), same vec4-per-column layout. Only read when `proof_enabled` is 1.
    pcol0: vec4<f32>,
    pcol1: vec4<f32>,
    pcol2: vec4<f32>,
    qcol0: vec4<f32>,
    qcol1: vec4<f32>,
    qcol2: vec4<f32>,
    // Screen-UV-to-texture-UV scale/offset (see this file's own header comment) -- vec2 has
    // align/size 8 in WGSL's uniform-address-space layout rules, placed before the scalar tail so
    // nothing needs an explicit alignment-bump pad the way a trailing vec3<u32> would (see below).
    view_scale: vec2<f32>,
    view_offset: vec2<f32>,
    // 1 when the render target is an `*Srgb` format: hardware then applies the sRGB OETF on
    // write, so the shader hands it linear values (`srgb_eotf` of the display-encoded result).
    target_srgb: u32,
    // 0 = exact matrix + built-in transfer function (`col0..2`, `trc`), 1 = baked 3D LUT,
    // 2 = matrix (`col0..2`) + per-channel encode table (a matrix/TRC monitor profile) (ADR-0042).
    mode: u32,
    // Mode 0 transfer function: 0 = sRGB curve (sRGB, Display P3), 1 = Adobe RGB gamma 563/256.
    trc: u32,
    // 1 = tint pixels outside the proof space's gamut (needs `proof_enabled`).
    gamut_warn: u32,
    proof_enabled: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
    // Scalar u32s, not a `vec4<u32>`/`vec3<u32>` pad -- a vecN field would itself need align
    // 16 in WGSL's uniform-address-space layout rules, pushing the struct's total size past what
    // the host-side `DisplayUniforms` (plain `u32`s, no vecN alignment) actually allocates -- a
    // real min-binding-size mismatch caught in review once already for this same struct. Matches
    // `nicti-tapetum::stages::LiveUniforms`'s own scalar-tail pattern for the same reason.
}

@group(0) @binding(0) var frame_tex: texture_2d<f32>;
@group(0) @binding(1) var frame_sampler: sampler;
@group(0) @binding(2) var<uniform> u: Uniforms;
// ADR-0042 monitor LUT (mode 1 only): 33^3, red = x, green = y, blue = z; rgb = display-encoded
// output. A 1^3 dummy is bound in mode 0.
@group(0) @binding(3) var lut_tex: texture_3d<f32>;
@group(0) @binding(4) var lut_sampler: sampler;
// ADR-0042 monitor encode table (mode 2 only): N x 1, texel i = per-channel encoded value at
// linear L = (i / (N-1))^2, i.e. indexed by sqrt(L) so the steep near-black part is sampled
// densely. A 1x1 dummy is bound otherwise.
@group(0) @binding(5) var trc_tex: texture_2d<f32>;

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
// A linear channel this far outside [0, 1] is float noise, not out of gamut (calico `GAMUT_EPS`).
const GAMUT_EPS: f32 = 0.002;
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

    var working = working_space.rgb;
    var out_of_gamut = false;
    if (u.proof_enabled != 0u) {
        // Exact relative-colorimetric proof: clip in the proof space's linear RGB, come back.
        let pm = mat3x3<f32>(u.pcol0.xyz, u.pcol1.xyz, u.pcol2.xyz);
        let qm = mat3x3<f32>(u.qcol0.xyz, u.qcol1.xyz, u.qcol2.xyz);
        let v = pm * working;
        out_of_gamut = any(v < vec3<f32>(-GAMUT_EPS)) || any(v > vec3<f32>(1.0 + GAMUT_EPS));
        working = qm * clamp(v, vec3<f32>(0.0), vec3<f32>(1.0));
    }

    var encoded: vec3<f32>;
    if (u.mode == 1u) {
        // Shaper (linear ProPhoto -> gamma 1.8), then trilinear LUT sample. Texel-centered
        // addressing: texel i sits at (i + 0.5) / N, so remap [0,1] onto that range.
        let n = f32(textureDimensions(lut_tex).x);
        let shaped = pow(clamp(working, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / PROPHOTO_GAMMA));
        let coord = shaped * ((n - 1.0) / n) + vec3<f32>(0.5 / n);
        encoded = textureSampleLevel(lut_tex, lut_sampler, coord, 0.0).rgb;
    } else if (u.mode == 2u) {
        let m = mat3x3<f32>(u.col0.xyz, u.col1.xyz, u.col2.xyz);
        let linear_out = clamp(m * working, vec3<f32>(0.0), vec3<f32>(1.0));
        // Index by sqrt(L), texel-centered, per channel.
        let tn = f32(textureDimensions(trc_tex).x);
        let x = sqrt(linear_out) * ((tn - 1.0) / tn) + vec3<f32>(0.5 / tn);
        encoded = vec3<f32>(
            textureSampleLevel(trc_tex, lut_sampler, vec2<f32>(x.x, 0.5), 0.0).r,
            textureSampleLevel(trc_tex, lut_sampler, vec2<f32>(x.y, 0.5), 0.0).g,
            textureSampleLevel(trc_tex, lut_sampler, vec2<f32>(x.z, 0.5), 0.0).b,
        );
    } else {
        let m = mat3x3<f32>(u.col0.xyz, u.col1.xyz, u.col2.xyz);
        let linear_out = m * working;
        encoded = vec3<f32>(
            encode_trc(linear_out.x),
            encode_trc(linear_out.y),
            encode_trc(linear_out.z),
        );
    }
    if (u.gamut_warn != 0u && out_of_gamut) {
        encoded = GAMUT_WARN;
    }

    if (u.target_srgb != 0u) {
        encoded = vec3<f32>(srgb_eotf(encoded.x), srgb_eotf(encoded.y), srgb_eotf(encoded.z));
    }
    return vec4<f32>(encoded, working_space.a);
}
