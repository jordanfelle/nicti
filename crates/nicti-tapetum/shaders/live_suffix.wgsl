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
    // #321: x = Look .xmp table enabled, y = Look table uses sRGB value encoding, z = profile tone
    // curve enabled, w unused.
    profile2: vec4<f32>,
    // #380 global Presence: x = texture, y = clarity, z = dehaze, w = saturation. Each is summed
    // with the stacked local delta of the same name (a global +0.3 and a local +0.2 act as +0.5).
    presence: vec4<f32>,
    // #428 defringe: (purple amount, purple hue lo, purple hue hi, green amount), then
    // (green hue lo, green hue hi, unused, unused). Amounts 0..1 (LRC 0..20), hues 0..1.
    defringe0: vec4<f32>,
    defringe1: vec4<f32>,
    // #432 point curves: x = enabled (point_curve_lut is only read when 1), yzw unused.
    point_curve: vec4<f32>,
    // #432 OkLab ops (oklab.rs is the CPU twin). ok_flags: x = any active, y = grading, z = point
    // colour. ok_to / ok_from: ProPhoto <-> LMS matrix rows. grade_k: (split, width). grade_w:
    // shadows / midtones / highlights / global offsets (dL, da, db). points: three vec4s per slot,
    // see stages.rs::pack_oklab.
    ok_flags: vec4<f32>,
    ok_to: array<vec4<f32>, 3>,
    ok_from: array<vec4<f32>, 3>,
    grade_k: vec4<f32>,
    grade_w: array<vec4<f32>, 4>,
    points: array<vec4<f32>, 24>,
}

@group(0) @binding(0) var input_tex: texture_2d<f32>;
@group(0) @binding(1) var output_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> u: Uniforms;
// DCP tables (#42): 3D textures, width = saturation, height = hue (wraps), depth = value, texel =
// (hue shift deg, sat scale, val scale). 1x1x1 dummies are bound when the profile lacks them.
@group(0) @binding(3) var hue_sat_map: texture_3d<f32>;
@group(0) @binding(4) var look_table: texture_3d<f32>;
@group(0) @binding(5) var profile_sampler: sampler;
// #321: the Adobe Raw "Look" .xmp profile's table, and the profile tone curve baked to
// TONE_LUT_LEN samples in sqrt space (see nicti_calico::tonecurve::ToneCurveLut). 1x1 dummies are
// bound when absent.
@group(0) @binding(11) var look_profile_table: texture_3d<f32>;
@group(0) @binding(12) var profile_tone_lut: texture_2d<f32>;
// #432: the point-curve LUTs, 256 x 3 R32Float (rows R, G, B; master already composed in), read
// with textureLoad. A never-read dummy is bound when the curves are the identity.
@group(0) @binding(13) var point_curve_lut: texture_2d<f32>;

// Local corrections (#49, mask/local.rs is the CPU twin). `mask_atlas` packs four correction
// composites per layer, one per channel, at the mask extent (sampled bilinearly). Each correction
// is five vec4s: (amount, exposure, contrast, highlights), (shadows, whites, blacks, temp),
// (tint, saturation, hue, noise), (tint multiplier rgb, sharpness), (clarity, texture, dehaze, _).
// A 1x1 one-layer dummy and count 0 are bound when there are none, and every local step below is
// skipped, so an unmasked image runs the exact pre-#49 path.
struct MaskUniforms {
    header: vec4<f32>,   // x = active correction count (<= 16), y = bands bound, z = haze bound
    airlight: vec4<f32>, // camera-linear airlight colour (dehaze)
    corr: array<vec4<f32>, 80>,
}
@group(0) @binding(6) var mask_atlas: texture_2d_array<f32>;
@group(0) @binding(7) var<uniform> mu: MaskUniforms;
@group(0) @binding(8) var mask_sampler: sampler;
// Cached per-photo bases (mask/bases.rs). `bases_tex` (Rgba16Float, bilinear) = (fine band, mid
// band, baked perceptual luma, _) for clarity/texture; `haze_tex` (R32Float, sampled by hand, 4-tap bilinear) = dehaze
// transmission. 1x1 dummies are bound when not needed; the header flags gate every read.
@group(0) @binding(9) var bases_tex: texture_2d<f32>;
@group(0) @binding(10) var haze_tex: texture_2d<f32>;

const CLARITY_GAIN: f32 = 1.5;
const TEXTURE_GAIN: f32 = 1.5;
const HAZE_T0: f32 = 0.1;
const VEIL_MAX: f32 = 0.5;

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
    noise: f32,
    sharpness: f32,
    clarity: f32,
    texture_amt: f32,
    dehaze: f32,
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
    s.noise = 0.0; s.sharpness = 0.0; s.clarity = 0.0; s.texture_amt = 0.0; s.dehaze = 0.0;
    let n = i32(mu.header.x);
    var layer = -1;
    var texel = vec4<f32>(0.0);
    for (var i: i32 = 0; i < n; i = i + 1) {
        let l = i / 4;
        if (l != layer) {
            texel = textureSampleLevel(mask_atlas, mask_sampler, uv, l, 0.0);
            layer = l;
        }
        let d0 = mu.corr[i * 5];
        let d1 = mu.corr[i * 5 + 1];
        let d2 = mu.corr[i * 5 + 2];
        let d3 = mu.corr[i * 5 + 3];
        let d4 = mu.corr[i * 5 + 4];
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
        s.noise = s.noise + f * d2.w;
        s.sharpness = s.sharpness + f * d3.w;
        s.clarity = s.clarity + f * d4.x;
        s.texture_amt = s.texture_amt + f * d4.y;
        s.dehaze = s.dehaze + f * d4.z;
    }
    return s;
}

// Mirrors mask/local.rs::apply_dehaze.
fn apply_dehaze(rgb: vec3<f32>, transmission: f32, airlight: vec3<f32>, amount: f32) -> vec3<f32> {
    if (amount > 0.0) {
        let t = max(1.0 - amount * (1.0 - transmission), HAZE_T0);
        return (rgb - airlight) / t + airlight;
    }
    let v = 1.0 - VEIL_MAX * min(-amount, 1.0);
    return rgb * v + airlight * (1.0 - v);
}

// Mirrors mask/local.rs::apply_bands: shift the baked perceptual luma by the mid/fine bands and
// apply the resulting brightness ratio (chroma-preserving), clamped to 0.25..4.
fn apply_bands(rgb: vec3<f32>, bands: vec4<f32>, clarity: f32, texture_amt: f32) -> vec3<f32> {
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let g_now = clamp(pow(max(luma, 0.0), 1.0 / 3.0), 0.0, 1.0);
    let mid = 4.0 * g_now * (1.0 - g_now);
    let g_base = max(bands.z, 1e-3);
    let g_new = max(g_base + clarity * CLARITY_GAIN * mid * bands.y + texture_amt * TEXTURE_GAIN * bands.x, 0.0);
    let ratio = clamp(pow(g_new / g_base, 2.2), 0.25, 4.0);
    return rgb * ratio;
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

fn point_curve_at(row: i32, index: i32) -> f32 {
    return textureLoad(point_curve_lut, vec2<i32>(clamp(index, 0, 255), row), 0).x;
}

// Mirrors color.rs::apply_point_curve exactly: per channel, cube-root perceptual space, linear
// interpolation between adjacent LUT entries.
fn apply_point_curve(rgb: vec3<f32>) -> vec3<f32> {
    let perceptual = clamp(pow(max(rgb, vec3<f32>(0.0)), vec3<f32>(1.0 / 3.0)), vec3<f32>(0.0), vec3<f32>(1.0));
    let pos = perceptual * 255.0;
    let i0 = vec3<i32>(floor(pos));
    let frac = pos - vec3<f32>(i0);
    let looked_up = vec3<f32>(
        mix(point_curve_at(0, i0.x), point_curve_at(0, i0.x + 1), frac.x),
        mix(point_curve_at(1, i0.y), point_curve_at(1, i0.y + 1), frac.y),
        mix(point_curve_at(2, i0.z), point_curve_at(2, i0.z + 1), frac.z),
    );
    let clamped = max(looked_up, vec3<f32>(0.0));
    return clamped * clamped * clamped;
}

fn ok_rows(m0: vec4<f32>, m1: vec4<f32>, m2: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(dot(m0.xyz, v), dot(m1.xyz, v), dot(m2.xyz, v));
}

fn ok_smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    if (hi <= lo) {
        return select(1.0, 0.0, x < lo);
    }
    let t = clamp((x - lo) / (hi - lo), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

fn ok_wrap_pi(a: f32) -> f32 {
    let tau = 6.283185307179586;
    let r = (a + 3.141592653589793);
    return (r - tau * floor(r / tau)) - 3.141592653589793;
}

fn ok_soft(half_width: f32, d: f32) -> f32 {
    return 1.0 - ok_smoothstep(0.5 * half_width, half_width, abs(d));
}

// Mirrors oklab.rs::OkLabOps::apply: ProPhoto -> OkLab, Color Grading, Point Color, back.
fn apply_oklab_ops(rgb: vec3<f32>) -> vec3<f32> {
    let lms = pow(max(ok_rows(u.ok_to[0], u.ok_to[1], u.ok_to[2], rgb), vec3<f32>(0.0)), vec3<f32>(1.0 / 3.0));
    // Ottosson's M2 (LMS' -> Lab).
    var lab = vec3<f32>(
        0.2104542553 * lms.x + 0.7936177850 * lms.y - 0.0040720468 * lms.z,
        1.9779984951 * lms.x - 2.4285922050 * lms.y + 0.4505937099 * lms.z,
        0.0259040371 * lms.x + 0.7827717662 * lms.y - 0.8086757660 * lms.z,
    );
    if (u.ok_flags.y > 0.5) {
        let split = u.grade_k.x;
        let width = u.grade_k.y;
        let ws = 1.0 - ok_smoothstep(split - width, split + 0.25 * width, lab.x);
        let wh = ok_smoothstep(split - 0.25 * width, split + width, lab.x);
        let wm = max(1.0 - ws - wh, 0.0);
        lab = lab
            + ws * u.grade_w[0].xyz
            + wm * u.grade_w[1].xyz
            + wh * u.grade_w[2].xyz
            + u.grade_w[3].xyz;
    }
    if (u.ok_flags.z > 0.5) {
        let l0 = lab.x;
        let c0 = length(lab.yz);
        var h0 = 0.0;
        if (c0 > 1e-6) {
            h0 = atan2(lab.z, lab.y);
        }
        var l = l0;
        var c = c0;
        var h = h0;
        for (var i = 0; i < 8; i = i + 1) {
            let p0 = u.points[i * 3];
            if (p0.w < 0.5) {
                continue;
            }
            let p1 = u.points[i * 3 + 1];
            let p2 = u.points[i * 3 + 2];
            let dh = ok_wrap_pi(h0 - p0.z);
            var hue_w = 1.0;
            if (p1.w > 0.5) {
                hue_w = ok_soft(p1.x, dh) * ok_smoothstep(0.005, 0.025, c0);
            }
            let w = hue_w * ok_soft(p1.y, c0 - p0.y) * ok_soft(p1.z, l0 - p0.x);
            if (w <= 0.0) {
                continue;
            }
            h = h + w * (p2.w * dh + p2.x);
            c = c + w * p2.w * (c0 - p0.y);
            c = c * (1.0 + w * p2.y);
            l = l + w * (p2.w * (l0 - p0.x) + p2.z);
        }
        c = max(c, 0.0);
        lab = vec3<f32>(l, c * cos(h), c * sin(h));
    }
    // Ottosson's M2 inverse (Lab -> LMS'), then cube.
    let lp = vec3<f32>(
        lab.x + 0.3963377774 * lab.y + 0.2158037573 * lab.z,
        lab.x - 0.1055613458 * lab.y - 0.0638541728 * lab.z,
        lab.x - 0.0894841775 * lab.y - 1.2914855480 * lab.z,
    );
    let back = ok_rows(u.ok_from[0], u.ok_from[1], u.ok_from[2], lp * lp * lp);
    return max(back, vec3<f32>(0.0));
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

fn profile_tone_eval(x: f32) -> f32 {
    let n = i32(textureDimensions(profile_tone_lut).x);
    let uu = sqrt(clamp(x, 0.0, 1.0)) * f32(n - 1);
    let i = min(i32(floor(uu)), n - 2);
    let t = uu - f32(i);
    let a = textureLoad(profile_tone_lut, vec2<i32>(i, 0), 0).x;
    let b = textureLoad(profile_tone_lut, vec2<i32>(i + 1, 0), 0).x;
    return a * (1.0 - t) + b * t;
}

// Hue-preserving RGB tone (nicti_calico::tonecurve::ToneCurveLut::apply_rgb): the largest and
// smallest channels go through the curve, the others are interpolated between them. Written with
// no dynamic vector indexing: FXC (Windows DX12) rejects `v[i] = x` with a runtime `i`.
fn profile_tone(rgb_in: vec3<f32>) -> vec3<f32> {
    let c = clamp(rgb_in, vec3<f32>(0.0), vec3<f32>(1.0));
    let mx = max(c.x, max(c.y, c.z));
    let mn = min(c.x, min(c.y, c.z));
    if (mx == mn) {
        return vec3<f32>(profile_tone_eval(c.x));
    }
    let yh = profile_tone_eval(mx);
    let yl = profile_tone_eval(mn);
    // The max channel gets t = 1 (yh), the min channel t = 0 (yl), ties included.
    let t = (c - vec3<f32>(mn)) / (mx - mn);
    return vec3<f32>(yl) + (yh - yl) * t;
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

// ---- #428 Defringe (color.rs::defringe_pixel is the CPU twin; keep the constants in sync) ----
// Idea and smoothstep bounds adapted from storytold/lightcraft@265248c crates/pipeline/src/optics.rs,
// Copyright (c) 2026 ArtCraft Team and the LightCraft contributors, MIT OR Apache-2.0 (see
// docs/licensing.md).
// Desaturates saturated purple/green pixels that sit next to a strong luminance edge. It runs
// right after the camera->working matrix, in scene-linear working space, so it sees the colours
// before any exposure/tone. The edge test reads 8 compass taps `r` px away in the baked input
// (cheap, and only reached when the hue/chroma gate already passed), where LightCraft used a full
// min/max box.
const DEFRINGE_EDGE_LO: f32 = 0.08;
const DEFRINGE_EDGE_HI: f32 = 0.25;
const DEFRINGE_SAT_LO: f32 = 0.05;
const DEFRINGE_SAT_HI: f32 = 0.20;
const DEFRINGE_SHOULDER: f32 = 8.0;
const DEFRINGE_PURPLE_BASE: f32 = 240.0;
const DEFRINGE_GREEN_BASE: f32 = 60.0;
const DEFRINGE_HUE_SPAN: f32 = 120.0;

fn perceptual_l(rgb: vec3<f32>) -> f32 {
    return pow(max(dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722)), 0.0), 1.0 / 3.0);
}

fn hue_window_at(h: f32, lo: f32, hi: f32) -> f32 {
    return smoothstep(lo - DEFRINGE_SHOULDER, lo, h) * (1.0 - smoothstep(hi, hi + DEFRINGE_SHOULDER, h));
}

// Also true across the 360/0 seam.
fn hue_window(h: f32, lo: f32, hi: f32) -> f32 {
    return max(hue_window_at(h, lo, hi), hue_window_at(h + 360.0, lo, hi));
}

// The perceptual L of the input pixel at `(x, y)` after the camera->working matrix, clamped to the
// frame.
fn tap_l(x: i32, y: i32, m: mat3x3<f32>) -> f32 {
    let d = vec2<i32>(textureDimensions(input_tex));
    let c = clamp(vec2<i32>(x, y), vec2<i32>(0, 0), d - vec2<i32>(1, 1));
    return perceptual_l(m * textureLoad(input_tex, c, 0).rgb);
}

fn apply_defringe(rgb: vec3<f32>, x: i32, y: i32, long_edge: u32, m: mat3x3<f32>) -> vec3<f32> {
    let hs = dcp_rgb_to_hsv(rgb);
    let strength_p = clamp(u.defringe0.x, 0.0, 1.0);
    let strength_g = clamp(u.defringe0.w, 0.0, 1.0);
    let purple = strength_p * hue_window(
        hs.x,
        DEFRINGE_PURPLE_BASE + DEFRINGE_HUE_SPAN * u.defringe0.y,
        DEFRINGE_PURPLE_BASE + DEFRINGE_HUE_SPAN * u.defringe0.z,
    );
    let green = strength_g * hue_window(
        hs.x,
        DEFRINGE_GREEN_BASE + DEFRINGE_HUE_SPAN * u.defringe1.x,
        DEFRINGE_GREEN_BASE + DEFRINGE_HUE_SPAN * u.defringe1.y,
    );
    let gate = max(purple, green) * smoothstep(DEFRINGE_SAT_LO, DEFRINGE_SAT_HI, hs.y);
    if (gate <= 0.0) {
        return rgb;
    }
    let r = i32(clamp(floor(2.0 * f32(long_edge) / 4000.0 + 0.5), 1.0, 6.0));
    let l = perceptual_l(rgb);
    var lo = l;
    var hi = l;
    var v = tap_l(x - r, y - r, m); lo = min(lo, v); hi = max(hi, v);
    v = tap_l(x, y - r, m); lo = min(lo, v); hi = max(hi, v);
    v = tap_l(x + r, y - r, m); lo = min(lo, v); hi = max(hi, v);
    v = tap_l(x - r, y, m); lo = min(lo, v); hi = max(hi, v);
    v = tap_l(x + r, y, m); lo = min(lo, v); hi = max(hi, v);
    v = tap_l(x - r, y + r, m); lo = min(lo, v); hi = max(hi, v);
    v = tap_l(x, y + r, m); lo = min(lo, v); hi = max(hi, v);
    v = tap_l(x + r, y + r, m); lo = min(lo, v); hi = max(hi, v);
    let k = gate * smoothstep(DEFRINGE_EDGE_LO, DEFRINGE_EDGE_HI, hi - lo);
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    return vec3<f32>(luma) + (rgb - vec3<f32>(luma)) * (1.0 - k);
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
    if (u.defringe0.x > 0.0 || u.defringe0.w > 0.0) {
        rgb = apply_defringe(rgb, i32(gid.x), i32(gid.y), max(dims.x, dims.y), m);
    }

    let has_locals = mu.header.x > 0.5;
    var locals: LocalSums;
    locals.exposure = 0.0; locals.contrast = 0.0; locals.highlights = 0.0; locals.shadows = 0.0;
    locals.whites = 0.0; locals.blacks = 0.0; locals.temp = 0.0; locals.tint = 0.0;
    locals.saturation = 0.0; locals.hue = 0.0; locals.tint_mult = vec3<f32>(0.0);
    locals.noise = 0.0; locals.sharpness = 0.0; locals.clarity = 0.0; locals.texture_amt = 0.0;
    locals.dehaze = 0.0;
    // The bases (and so `uv`) are also needed for a global clarity/texture/dehaze with no mask at
    // all: the header's bands/haze flags say whether the caller bound them.
    let uv = (vec2<f32>(f32(gid.x), f32(gid.y)) + vec2<f32>(0.5)) / vec2<f32>(f32(dims.x), f32(dims.y));
    if (has_locals) {
        locals = accumulate_locals(uv);
    }
    let dehaze_total = locals.dehaze + u.presence.z;
    let clarity_total = locals.clarity + u.presence.y;
    let texture_total = locals.texture_amt + u.presence.x;
    let saturation_total = locals.saturation + u.presence.w;

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
    if (u.profile2.x > 0.5) {
        rgb = dcp_apply_table(rgb, look_profile_table, u.profile2.y > 0.5);
    }
    if (u.profile2.z > 0.5) {
        rgb = profile_tone(rgb);
    }
    // Local dehaze: scene-linear, before white balance / tone. The airlight goes through the same
    // matrix and exposure as the pixels it is subtracted from.
    if (mu.header.z > 0.5 && dehaze_total != 0.0) {
        // The transmission lives at the bases extent (<= 2048 px long edge) but the live pass runs
        // at the frame extent, and the guided refine makes `t` change sharply at edges, so a
        // nearest-texel load would step by one bases texel. R32Float isn't filterable: 4 taps by
        // hand, with the same pixel-centre mapping as guided_apply (at equal extents this reduces
        // exactly to the texel, so the CPU twin still matches).
        let hd = vec2<f32>(textureDimensions(haze_tex));
        let hmax = vec2<i32>(hd) - vec2<i32>(1);
        let hf = uv * hd - vec2<f32>(0.5);
        let hb = floor(hf);
        let hr = hf - hb;
        let h0 = clamp(vec2<i32>(hb), vec2<i32>(0), hmax);
        let h1 = clamp(vec2<i32>(hb) + vec2<i32>(1), vec2<i32>(0), hmax);
        let t00 = textureLoad(haze_tex, h0, 0).r;
        let t10 = textureLoad(haze_tex, vec2<i32>(h1.x, h0.y), 0).r;
        let t01 = textureLoad(haze_tex, vec2<i32>(h0.x, h1.y), 0).r;
        let t11 = textureLoad(haze_tex, h1, 0).r;
        let t = mix(mix(t00, t10, hr.x), mix(t01, t11, hr.x), hr.y);
        rgb = apply_dehaze(rgb, t, (m * mu.airlight.xyz) * exposure, dehaze_total);
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
    if (u.point_curve.x > 0.5) {
        rgb = apply_point_curve(rgb);
    }
    if (mu.header.y > 0.5 && (clarity_total != 0.0 || texture_total != 0.0)) {
        let bands = textureSampleLevel(bases_tex, mask_sampler, uv, 0.0);
        rgb = apply_bands(rgb, bands, clarity_total, texture_total);
    }
    rgb = apply_vibrance(rgb, u.tone1.z);
    rgb = apply_hsl(rgb);
    if (u.ok_flags.x > 0.5) {
        rgb = apply_oklab_ops(rgb);
    }
    // Skipped when the stacked delta is exactly zero, so a mask that selects nothing (or has no
    // such adjustment) leaves the pixel bit-identical, not merely close.
    if (saturation_total != 0.0) {
        rgb = saturate_chroma(rgb, saturation_total);
    }
    if (has_locals) {
        if (locals.hue != 0.0) {
            rgb = rotate_hue(rgb, locals.hue * HUE_DEGREES);
        }
        if (any(locals.tint_mult != vec3<f32>(0.0))) {
            rgb = rgb * max(vec3<f32>(1.0) + locals.tint_mult, vec3<f32>(0.0));
        }
    }
    textureStore(output_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(rgb, px.a));
}
