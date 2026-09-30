// The fused live suffix (ADR-0044's "one fused live dispatch"): white balance + camera->working
// -space color (folded into one 3x3 on the CPU, see color.rs::camera_to_working_space_matrix),
// exposure, Basic-panel tone (contrast/highlights/shadows/whites/blacks), #46's Tone Curve, #46's
// 8-band HSL, and vibrance -- exactly one dispatch regardless of how many of these params changed.
// Output stays in linear ProPhoto RGB (the working space); no display/export color management
// here (that lives in nicti-pelt's display pass, ADR-0042). #42 adds the DCP camera profile's
// HueSatMap -> baseline exposure -> LookTable stages right after the camera->working matrix (the
// ADR-0038 stage order); each is skipped, costing nothing, when the profile lacks it. #46's own
// HSL below is the user-facing HSL panel, a different thing from the profile-driven HueSatMap.
//
// #46's Sharpening/Noise Reduction are a separate pass (`detail_blur.wgsl`/`detail_combine.wgsl`,
// see `stages.rs::LiveSuffixKernel::encode`'s own doc comment) -- unlike everything in this file,
// they need neighboring pixels, which a per-pixel-only shader like this one can't provide.

struct Uniforms {
    // Row-major 3x3 camera-RGB -> working-space matrix, one column per vec4 (w unused, alignment
    // only) to avoid relying on WGSL's mat3x3 uniform-buffer layout matching Rust's.
    col0: vec4<f32>,
    col1: vec4<f32>,
    col2: vec4<f32>,
    // exposure_mult, contrast, highlights, shadows.
    tone0: vec4<f32>,
    // whites, blacks, vibrance, unused.
    tone1: vec4<f32>,
    // Tone Curve LUT, 256 entries packed 4-per-vec4 (see color.rs::build_tone_curve_lut).
    curve_lut: array<vec4<f32>, 64>,
    // HSL panel's 8 bands, one vec4 each: hue, saturation, luminance, unused.
    hsl_bands: array<vec4<f32>, 8>,
    // DCP camera profile (#42): x = HueSatMap enabled, y = LookTable enabled, z = HueSatMap uses
    // sRGB value encoding, w = LookTable uses sRGB value encoding (all 0/1).
    profile0: vec4<f32>,
    // x = baseline-exposure multiplier (2^BaselineExposureOffset; 1.0 with no profile).
    profile1: vec4<f32>,
}

@group(0) @binding(0) var input_tex: texture_2d<f32>;
@group(0) @binding(1) var output_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> u: Uniforms;
// DCP tables (#42): 3D textures, width = saturation, height = hue (wraps), depth = value, texel =
// (hue shift deg, sat scale, val scale). 1x1x1 dummies are bound when the profile lacks them.
@group(0) @binding(3) var hue_sat_map: texture_3d<f32>;
@group(0) @binding(4) var look_table: texture_3d<f32>;
@group(0) @binding(5) var profile_sampler: sampler;

// Local corrections (#49, mask/local.rs is the CPU twin). `mask_atlas` packs four correction
// composites per layer, one per channel, at the mask extent (sampled bilinearly). Each correction
// is four vec4s: (amount, exposure, contrast, highlights), (shadows, whites, blacks, temp),
// (tint, saturation, hue, _), (tint multiplier rgb, _). A 1x1 one-layer dummy and count 0 are
// bound when there are none, and every local step below is skipped, so an unmasked image runs the
// exact pre-#49 path.
struct MaskUniforms {
    header: vec4<f32>, // x = active correction count (<= 16)
    corr: array<vec4<f32>, 64>,
}
@group(0) @binding(6) var mask_atlas: texture_2d_array<f32>;
@group(0) @binding(7) var<uniform> mu: MaskUniforms;
@group(0) @binding(8) var mask_sampler: sampler;

const TEMP_STOPS: f32 = 0.5;
const TINT_STOPS: f32 = 0.25;
const HUE_DEGREES: f32 = 30.0;

struct LocalSums {
    exposure: f32,
    contrast: f32,
    highlights: f32,
    shadows: f32,
    whites: f32,
    blacks: f32,
    temp: f32,
    tint: f32,
    saturation: f32,
    hue: f32,
    tint_mult: vec3<f32>,
}

fn channel_of(v: vec4<f32>, c: i32) -> f32 {
    if (c == 0) { return v.x; }
    if (c == 1) { return v.y; }
    if (c == 2) { return v.z; }
    return v.w;
}

// weight_i * amount_i * delta_i, summed over the active corrections (LRC's additive stacking).
fn accumulate_locals(uv: vec2<f32>) -> LocalSums {
    var s: LocalSums;
    s.exposure = 0.0; s.contrast = 0.0; s.highlights = 0.0; s.shadows = 0.0; s.whites = 0.0;
    s.blacks = 0.0; s.temp = 0.0; s.tint = 0.0; s.saturation = 0.0; s.hue = 0.0;
    s.tint_mult = vec3<f32>(0.0);
    let n = i32(mu.header.x);
    var layer = -1;
    var texel = vec4<f32>(0.0);
    for (var i: i32 = 0; i < n; i = i + 1) {
        let l = i / 4;
        if (l != layer) {
            texel = textureSampleLevel(mask_atlas, mask_sampler, uv, l, 0.0);
            layer = l;
        }
        let d0 = mu.corr[i * 4];
        let d1 = mu.corr[i * 4 + 1];
        let d2 = mu.corr[i * 4 + 2];
        let d3 = mu.corr[i * 4 + 3];
        let f = channel_of(texel, i % 4) * d0.x;
        s.exposure = s.exposure + f * d0.y;
        s.contrast = s.contrast + f * d0.z;
        s.highlights = s.highlights + f * d0.w;
        s.shadows = s.shadows + f * d1.x;
        s.whites = s.whites + f * d1.y;
        s.blacks = s.blacks + f * d1.z;
        s.temp = s.temp + f * d1.w;
        s.tint = s.tint + f * d2.x;
        s.saturation = s.saturation + f * d2.y;
        s.hue = s.hue + f * d2.z;
        s.tint_mult = s.tint_mult + f * d3.xyz;
    }
    return s;
}

// Rodrigues rotation about the grey axis (1,1,1)/sqrt(3) -- mirrors mask/local.rs::rotate_hue.
fn rotate_hue(rgb: vec3<f32>, degrees: f32) -> vec3<f32> {
    let a = radians(degrees);
    let sn = sin(a);
    let cs = cos(a);
    let k = 0.5773502691896258;
    let d = (rgb.x + rgb.y + rgb.z) * k;
    let cr = vec3<f32>(k * (rgb.z - rgb.y), k * (rgb.x - rgb.z), k * (rgb.y - rgb.x));
    return cs * rgb + sn * cr + vec3<f32>((1.0 - cs) * d * k);
}

// Luma-preserving chroma scale -- mirrors mask/local.rs::saturate.
fn saturate_chroma(rgb: vec3<f32>, amount: f32) -> vec3<f32> {
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let k = max(1.0 + amount, 0.0);
    return vec3<f32>(luma) + (rgb - vec3<f32>(luma)) * k;
}

const PI: f32 = 3.14159265358979;

// Basic-panel tone controls in a rough perceptual (cube-root) space -- see color.rs::apply_tone's
// own doc comment for what each control does and why this is a v1 global approximation.
fn apply_tone(rgb: vec3<f32>, contrast: f32, highlights: f32, shadows: f32, whites: f32, blacks: f32) -> vec3<f32> {
    // Positive `whites` moves the white point *down* (brighter highlights); negative `blacks`
    // moves the black point *up* (darker, crushed shadows) -- see color.rs::apply_tone's own
    // comment for why this isn't a naive same-sign offset.
    let white_point = 1.0 - whites * 0.3;
    let black_point = -blacks * 0.3;
    let range = max(white_point - black_point, 1e-4);

    let c = max(rgb, vec3<f32>(0.0));
    let perceptual = pow(c, vec3<f32>(1.0 / 3.0));
    let remapped = (perceptual - vec3<f32>(black_point)) / range;
    let contrasted = (remapped - vec3<f32>(0.5)) * (1.0 + contrast) + vec3<f32>(0.5);

    let luma = dot(contrasted, vec3<f32>(0.2126, 0.7152, 0.0722));
    let hi_w = smoothstep(0.35, 0.9, luma);
    let sh_w = 1.0 - smoothstep(0.1, 0.65, luma);
    let shifted = contrasted + vec3<f32>(highlights * 0.25 * hi_w + shadows * 0.25 * sh_w);

    let clamped = max(shifted, vec3<f32>(0.0));
    return clamped * clamped * clamped;
}

// One LUT entry, given a 0..255 integer index -- WGSL can't dynamically index a vec4's
// components, so this unpacks the 4-per-vec4 packing color.rs::build_tone_curve_lut's own doc
// comment describes.
fn lut_at(index: i32) -> f32 {
    let clamped = clamp(index, 0, 255);
    let group = u.curve_lut[clamped / 4];
    let comp = clamped % 4;
    if (comp == 0) { return group.x; }
    if (comp == 1) { return group.y; }
    if (comp == 2) { return group.z; }
    return group.w;
}

// Mirrors color.rs::apply_tone_curve exactly: per-channel, in the same cube-root perceptual space
// apply_tone uses, with linear interpolation between adjacent LUT entries.
fn apply_tone_curve(rgb: vec3<f32>) -> vec3<f32> {
    let perceptual = clamp(pow(max(rgb, vec3<f32>(0.0)), vec3<f32>(1.0 / 3.0)), vec3<f32>(0.0), vec3<f32>(1.0));
    let pos = perceptual * 255.0;
    let i0 = vec3<i32>(floor(pos));
    let frac = pos - vec3<f32>(i0);
    let looked_up = vec3<f32>(
        mix(lut_at(i0.x), lut_at(i0.x + 1), frac.x),
        mix(lut_at(i0.y), lut_at(i0.y + 1), frac.y),
        mix(lut_at(i0.z), lut_at(i0.z + 1), frac.z),
    );
    let clamped = max(looked_up, vec3<f32>(0.0));
    return clamped * clamped * clamped;
}

fn hsl_band(index: i32) -> vec3<f32> {
    return u.hsl_bands[index].xyz;
}

// Standard "which channel is max" hue formula -- mirrors color.rs::rgb_hue_degrees exactly.
// Undefined (returns 0.0) when delta is ~0; callers must not call this on an achromatic pixel.
fn rgb_hue_degrees(rgb: vec3<f32>, mx: f32, delta: f32) -> f32 {
    var raw: f32;
    if (mx == rgb.r) {
        raw = ((rgb.g - rgb.b) / delta) % 6.0;
    } else if (mx == rgb.g) {
        raw = (rgb.b - rgb.r) / delta + 2.0;
    } else {
        raw = (rgb.r - rgb.g) / delta + 4.0;
    }
    if (raw < 0.0) {
        raw = raw + 6.0;
    }
    var deg = raw * 60.0;
    deg = deg % 360.0;
    if (deg < 0.0) {
        deg = deg + 360.0;
    }
    return deg;
}

fn hue_delta_degrees(hue: f32, center: f32) -> f32 {
    var raw = (hue - center) % 360.0;
    if (raw < 0.0) {
        raw = raw + 360.0;
    }
    if (raw > 180.0) {
        raw = raw - 360.0;
    }
    return raw;
}

// Mirrors color.rs::hsl_band_weight exactly: a raised-cosine (Hann) window spanning +/-45
// degrees around a band's own 45-degree-spaced center.
fn hsl_band_weight(hue: f32, band_index: i32) -> f32 {
    let center = f32(band_index) * 45.0;
    let d = abs(hue_delta_degrees(hue, center));
    if (d >= 45.0) {
        return 0.0;
    }
    return 0.5 * (1.0 + cos(PI * d / 45.0));
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> vec3<f32> {
    let c = v * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - abs((h_prime % 2.0) - 1.0));
    var rgb: vec3<f32>;
    if (h_prime < 1.0) {
        rgb = vec3<f32>(c, x, 0.0);
    } else if (h_prime < 2.0) {
        rgb = vec3<f32>(x, c, 0.0);
    } else if (h_prime < 3.0) {
        rgb = vec3<f32>(0.0, c, x);
    } else if (h_prime < 4.0) {
        rgb = vec3<f32>(0.0, x, c);
    } else if (h_prime < 5.0) {
        rgb = vec3<f32>(x, 0.0, c);
    } else {
        rgb = vec3<f32>(c, 0.0, x);
    }
    let m = v - c;
    return rgb + vec3<f32>(m);
}

// Mirrors color.rs::apply_hsl exactly -- see its own doc comment for the HSV-plus-separate-
// perceptual-luma-shift approximation this uses instead of canonical HSL.
fn apply_hsl(rgb: vec3<f32>) -> vec3<f32> {
    let mx = max(rgb.r, max(rgb.g, rgb.b));
    let mn = min(rgb.r, min(rgb.g, rgb.b));
    let delta = mx - mn;
    if (delta <= 1e-6 || mx <= 0.0) {
        return rgb;
    }

    let hue = rgb_hue_degrees(rgb, mx, delta);
    var hue_shift = 0.0;
    var sat_shift = 0.0;
    var luma_shift = 0.0;
    for (var i: i32 = 0; i < 8; i = i + 1) {
        let w = hsl_band_weight(hue, i);
        let band = hsl_band(i);
        hue_shift = hue_shift + w * band.x;
        sat_shift = sat_shift + w * band.y;
        luma_shift = luma_shift + w * band.z;
    }
    hue_shift = hue_shift * 30.0;
    luma_shift = luma_shift * 0.3;

    let sat = clamp(delta / mx, 0.0, 1.0);
    var new_hue = (hue + hue_shift) % 360.0;
    if (new_hue < 0.0) {
        new_hue = new_hue + 360.0;
    }
    let new_sat = max(sat * (1.0 + sat_shift), 0.0);
    let hue_rotated = hsv_to_rgb(new_hue, new_sat, mx);

    let perceptual = pow(max(hue_rotated, vec3<f32>(0.0)), vec3<f32>(1.0 / 3.0));
    let shifted = max(perceptual + vec3<f32>(luma_shift), vec3<f32>(0.0));
    return shifted * shifted * shifted;
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

// DNG-spec HSV (hue in degrees). Unclamped in v: linear ProPhoto values above 1 are legitimate
// here (saturated colors before the tone stages bring them down).
fn dcp_rgb_to_hsv(rgb: vec3<f32>) -> vec3<f32> {
    let max_c = max(rgb.r, max(rgb.g, rgb.b));
    let min_c = min(rgb.r, min(rgb.g, rgb.b));
    let delta = max_c - min_c;
    var h: f32 = 0.0;
    if (delta > 1e-6) {
        if (max_c == rgb.r) {
            h = 60.0 * (((rgb.g - rgb.b) / delta) % 6.0);
        } else if (max_c == rgb.g) {
            h = 60.0 * ((rgb.b - rgb.r) / delta + 2.0);
        } else {
            h = 60.0 * ((rgb.r - rgb.g) / delta + 4.0);
        }
    }
    if (h < 0.0) {
        h = h + 360.0;
    }
    let s = select(0.0, delta / max_c, max_c > 0.0);
    return vec3<f32>(h, s, max_c);
}

fn dcp_hsv_to_rgb(hsv: vec3<f32>) -> vec3<f32> {
    // Wrap into [0, 360): hue + a table shift can cross the seam, and WGSL's `%` keeps the
    // dividend's sign, so an unwrapped negative hue would pick the wrong sector below.
    let h = hsv.x - 360.0 * floor(hsv.x / 360.0);
    let s = clamp(hsv.y, 0.0, 1.0);
    let v = hsv.z;
    let c = v * s;
    let hp = h / 60.0;
    let x = c * (1.0 - abs((hp % 2.0) - 1.0));
    var rgb1: vec3<f32>;
    let sector = i32(hp);
    if (sector == 0) {
        rgb1 = vec3<f32>(c, x, 0.0);
    } else if (sector == 1) {
        rgb1 = vec3<f32>(x, c, 0.0);
    } else if (sector == 2) {
        rgb1 = vec3<f32>(0.0, c, x);
    } else if (sector == 3) {
        rgb1 = vec3<f32>(0.0, x, c);
    } else if (sector == 4) {
        rgb1 = vec3<f32>(x, 0.0, c);
    } else {
        rgb1 = vec3<f32>(c, 0.0, x);
    }
    let m = v - c;
    return rgb1 + vec3<f32>(m, m, m);
}

fn dcp_srgb_oetf(c: f32) -> f32 {
    let v = max(c, 0.0);
    if (v <= 0.0031308) {
        return v * 12.92;
    }
    return 1.055 * pow(v, 1.0 / 2.4) - 0.055;
}

fn dcp_srgb_eotf(c: f32) -> f32 {
    let v = max(c, 0.0);
    if (v <= 0.04045) {
        return v / 12.92;
    }
    return pow((v + 0.055) / 1.055, 2.4);
}

// One HueSatMap/LookTable (DNG SDK RefBaselineHueSatMap): hue and saturation come from the
// *unencoded* linear RGB; only the value coordinate goes through the table's encoding (sRGB when
// `srgb_value`), for both the lookup and the returned scale, then decoded back. Trilinear
// filtering treats texel i's center as (i+0.5)/N, so coordinates are remapped (hue tiles: i/N;
// sat/value span edge to edge: i/(N-1)). The hardware lerps the hue shift linearly rather than
// along the shortest arc -- an accepted approximation (the CPU reference in nicti-calico differs
// only across the 0/360 seam).
fn dcp_apply_table(rgb: vec3<f32>, tex: texture_3d<f32>, srgb_value: bool) -> vec3<f32> {
    let hsv = dcp_rgb_to_hsv(rgb);
    var v_enc = max(hsv.z, 0.0);
    if (srgb_value) {
        v_enc = dcp_srgb_oetf(hsv.z);
    }
    let dims = vec3<f32>(textureDimensions(tex));
    let cu = select((hsv.y * (dims.x - 1.0) + 0.5) / dims.x, 0.5, dims.x <= 1.0);
    let cv = hsv.x / 360.0 + 0.5 / dims.y;
    let cw = select((clamp(v_enc, 0.0, 1.0) * (dims.z - 1.0) + 0.5) / dims.z, 0.5, dims.z <= 1.0);
    let adj = textureSampleLevel(tex, profile_sampler, vec3<f32>(cu, cv, cw), 0.0).xyz;
    var v_out = v_enc * adj.z;
    if (srgb_value) {
        v_out = dcp_srgb_eotf(v_out);
    }
    return dcp_hsv_to_rgb(vec3<f32>(hsv.x + adj.x, clamp(hsv.y * adj.y, 0.0, 1.0), v_out));
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(input_tex);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let px = textureLoad(input_tex, vec2<i32>(i32(gid.x), i32(gid.y)), 0);
    let m = mat3x3<f32>(u.col0.xyz, u.col1.xyz, u.col2.xyz);
    var rgb = m * px.rgb;

    let has_locals = mu.header.x > 0.5;
    var locals: LocalSums;
    locals.exposure = 0.0; locals.contrast = 0.0; locals.highlights = 0.0; locals.shadows = 0.0;
    locals.whites = 0.0; locals.blacks = 0.0; locals.temp = 0.0; locals.tint = 0.0;
    locals.saturation = 0.0; locals.hue = 0.0; locals.tint_mult = vec3<f32>(0.0);
    if (has_locals) {
        let uv = (vec2<f32>(f32(gid.x), f32(gid.y)) + vec2<f32>(0.5)) / vec2<f32>(f32(dims.x), f32(dims.y));
        locals = accumulate_locals(uv);
    }

    // DCP camera profile (ADR-0038 order): HueSatMap -> baseline exposure -> LookTable.
    if (u.profile0.x > 0.5) {
        rgb = dcp_apply_table(rgb, hue_sat_map, u.profile0.z > 0.5);
    }
    // Baseline exposure and the user's Exposure slider are one stage (Adobe's `dng_render`): the
    // LookTable must see the exposed image. With no profile this is just the user exposure. A
    // local exposure is part of the same stage (its delta is in stops).
    var exposure = u.profile1.x * u.tone0.x;
    if (has_locals) {
        exposure = exposure * exp2(locals.exposure);
    }
    rgb = rgb * exposure;
    if (u.profile0.y > 0.5) {
        rgb = dcp_apply_table(rgb, look_table, u.profile0.w > 0.5);
    }
    var contrast = u.tone0.y;
    var highlights = u.tone0.z;
    var shadows = u.tone0.w;
    var whites = u.tone1.x;
    var blacks = u.tone1.y;
    if (has_locals) {
        // Local white balance: per-channel gains in linear working space, then the stacked tone.
        rgb = rgb * vec3<f32>(
            exp2(TEMP_STOPS * locals.temp),
            exp2(-TINT_STOPS * locals.tint),
            exp2(-TEMP_STOPS * locals.temp),
        );
        contrast = clamp(contrast + locals.contrast, -1.0, 2.0);
        highlights = clamp(highlights + locals.highlights, -2.0, 2.0);
        shadows = clamp(shadows + locals.shadows, -2.0, 2.0);
        whites = clamp(whites + locals.whites, -2.0, 2.0);
        blacks = clamp(blacks + locals.blacks, -2.0, 2.0);
    }
    rgb = apply_tone(rgb, contrast, highlights, shadows, whites, blacks);
    rgb = apply_tone_curve(rgb);
    rgb = apply_vibrance(rgb, u.tone1.z);
    rgb = apply_hsl(rgb);
    if (has_locals) {
        rgb = saturate_chroma(rgb, locals.saturation);
        rgb = rotate_hue(rgb, locals.hue * HUE_DEGREES);
        rgb = rgb * max(vec3<f32>(1.0) + locals.tint_mult, vec3<f32>(0.0));
    }
    textureStore(output_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(rgb, px.a));
}
