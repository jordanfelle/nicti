// Fullscreen-triangle blit from a Tapetum `FrameTexture` (linear ProPhoto RGB, Rgba16Float) to
// egui's own render-pass color target. Matches `nicti_tapetum::geometry::output_encode`'s CPU
// reference exactly: working-space -> linear sRGB via the ProPhoto->sRGB matrix, then the sRGB
// OETF -- except the OETF is skipped when the target itself is an `*Srgb` format, since the
// hardware already applies it on write in that case (applying it twice would double-gamma the
// image). `u.apply_oetf` is 0/1 rather than a `bool` -- WGSL uniform buffers don't guarantee a
// `bool`'s host-side layout, `u32` does.
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
    apply_oetf: u32,
    // Three scalar u32 pads, not a `vec3<u32>` -- a vecN pad field would itself need align 16 in
    // WGSL's uniform-address-space layout rules (same as vec4), pushing this field (and the
    // struct's total size) past what the host-side `DisplayUniforms` (plain `u32` pads, no vecN
    // alignment) actually allocates -- a real min-binding-size mismatch caught in review once
    // already for this same struct, not just a style choice. Matches
    // `nicti-tapetum::stages::LiveUniforms`'s own scalar-tail pattern for the same reason.
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

@group(0) @binding(0) var frame_tex: texture_2d<f32>;
@group(0) @binding(1) var frame_sampler: sampler;
@group(0) @binding(2) var<uniform> u: Uniforms;

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
