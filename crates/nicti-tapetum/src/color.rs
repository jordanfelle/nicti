//! CPU-side color math for the live suffix: white balance (as-shot, or a manual temp/tint
//! override, #46), the camera-RGB -> working-space (linear ProPhoto RGB) matrix chain, Basic-panel
//! tone controls, and a vibrance formula. Shared between building the live-suffix shader's uniform
//! values and a pure-CPU reference used to prove the GPU kernel matches it (this crate's own
//! goldens, before real ones exist in #45's tiling slice).
//!
//! **Deliberate simplification vs. the original design sketch**: no `HueSatMap`/`LookTable`
//! bindings are reserved in the shader (#42's DCP-profile color pipeline). Wiring in unused
//! texture bindings with no real content to sample would be exactly the kind of half-finished
//! scaffolding this repo's own conventions ask to avoid -- #42 can extend the shader (and this
//! module) when it lands, without needing this pipeline's *shape* pre-declared for it. Likewise,
//! the tone curve here (see [`apply_tone`]) is a parametric formula rather than a 1D LUT texture --
//! real, but intentionally the smallest thing that proves the pipeline end-to-end; #46's own tone-
//! curve slice swaps in a LUT without changing any other stage.
//!
//! **WB temp/tint is also a v1 simplification**: [`wb_gains_for_temp_tint`] estimates camera-space
//! gains from a single `LinearFrame::cam_xyz` matrix, not the DNG spec's own CCT-interpolated
//! dual-illuminant solve (`spikes/calico::cct` implements the real thing, for when a DCP profile
//! is available -- #42's still-Proposed scope). Good enough for a manual WB slider to move the
//! image in the expected direction; not claimed to match Adobe's own temp/tint numbers exactly.

use crate::coat::{
    DefringeParams, HslParams, PointCurveParams, ToneCurveParams, ToneParams, WbParams,
};

pub type Mat3 = [[f32; 3]; 3];

pub fn mat3_identity() -> Mat3 {
    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
}

/// Row-major 3x3 * 3x3 (standard matrix multiplication: `mat3_mul(a, b)` applies `b` first, then
/// `a`, matching function-composition order).
pub fn mat3_mul(a: Mat3, b: Mat3) -> Mat3 {
    let mut out = [[0.0f32; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            out[r][c] = (0..3).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    out
}

pub fn mat3_apply(m: Mat3, v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

fn mat3_diag(d: [f32; 3]) -> Mat3 {
    [[d[0], 0.0, 0.0], [0.0, d[1], 0.0], [0.0, 0.0, d[2]]]
}

/// Cramer's-rule 3x3 inverse. Panics on a singular matrix -- `cam_xyz` (the only matrix this
/// module inverts) is always invertible in practice: LibRaw derives it from a real sensor's
/// spectral-sensitivity calibration, never a degenerate/rank-deficient one.
pub(crate) fn mat3_invert(m: &Mat3) -> Mat3 {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    assert!(det.abs() > 1e-12, "cannot invert a singular matrix");
    let inv_det = 1.0 / det;
    [
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv_det,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv_det,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv_det,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv_det,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv_det,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv_det,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv_det,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv_det,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv_det,
        ],
    ]
}

/// The first three rows/cols of `nicti_cornea::LinearFrame::cam_xyz` (row-major 4x3) as a 3x3 --
/// the 4th row/col (LibRaw's G2 channel) is never populated by `LinearFrame`, which already drops
/// G2 during decode.
///
/// **Direction**: per LibRaw's own `cam_xyz_coeff` (`utils_dcraw.cpp`, confirmed against the
/// vendored source, not just its header comment), `cam_xyz` maps **XYZ -> camera** RGB, the
/// opposite of what its name suggests read as "camera to XYZ" -- `cam_rgb[i][j] = cam_xyz[i][k] *
/// xyz_rgb[k][j]`, i.e. `cam_xyz` composes with an XYZ input, never a camera one. Callers that
/// need camera -> XYZ (every caller in this crate) must invert the 3x3 this function returns; see
/// [`camera_to_working_space_matrix`].
pub fn cam_xyz_to_mat3(cam_xyz: &[f32; 12]) -> Mat3 {
    [
        [cam_xyz[0], cam_xyz[1], cam_xyz[2]],
        [cam_xyz[3], cam_xyz[4], cam_xyz[5]],
        [cam_xyz[6], cam_xyz[7], cam_xyz[8]],
    ]
}

/// As-shot white-balance gains from `cam_mul` (R/G/B/G2), normalized so the green gain is 1.0 --
/// the conventional "ratio over as-shot" framing (a gain of 1.0 on every channel is a no-op WB).
pub fn wb_gains(cam_mul: [f32; 4]) -> [f32; 3] {
    let g = if cam_mul[1] != 0.0 { cam_mul[1] } else { 1.0 };
    [cam_mul[0] / g, 1.0, cam_mul[2] / g]
}

/// Kim et al. 2002's Planckian-locus xy approximation for a given CCT -- the same published
/// formula `spikes/calico::cct::cct_to_approx_xy` uses (duplicated here, not depended on, since
/// that crate's `Mat3`/`Vec3` are `f64` and not a workspace dependency of this crate). Computed in
/// `f64` (the formula's published coefficients need more precision than `f32` carries) and cast
/// down to `f32` only in the result, matching every other coordinate this module works in.
fn planckian_locus_xy(cct: f32) -> (f32, f32) {
    let t = (cct as f64).clamp(1667.0, 25000.0);
    let x = if t <= 4000.0 {
        -0.2661239e9 / t.powi(3) - 0.2343589e6 / t.powi(2) + 0.8776956e3 / t + 0.179910
    } else {
        -3.0258469e9 / t.powi(3) + 2.1070379e6 / t.powi(2) + 0.2226347e3 / t + 0.240390
    };
    let y = if t <= 2222.0 {
        -1.1063814 * x.powi(3) - 1.34811020 * x.powi(2) + 2.18555832 * x - 0.20219683
    } else if t <= 4000.0 {
        -0.9549476 * x.powi(3) - 1.37418593 * x.powi(2) + 2.09137015 * x - 0.16748867
    } else {
        3.0817580 * x.powi(3) - 5.87338670 * x.powi(2) + 3.75112997 * x - 0.37001483
    };
    (x as f32, y as f32)
}

/// Camera-space WB gains for a manually chosen temperature/tint, from this frame's own single
/// `cam_xyz` matrix -- see this module's own doc comment for why this is a v1 approximation, not
/// the DNG spec's dual-illuminant solve. `tint`'s effect (a green-magenta shift) is applied as a
/// small perpendicular offset in xy space, scaled to stay plausible across PV2012's -150..150
/// range; `temp_k` moves along the Planckian locus.
/// Clamps a division's denominator away from zero without flipping its sign -- `.max(epsilon)`
/// alone silently turns any negative value (a real, reachable case for `camera_neutral`'s off-
/// diagonal-heavy real camera matrices) into a small *positive* one, producing a wrong-signed,
/// wildly-oversized gain instead of a merely-clamped one.
fn clamp_denominator(value: f32, epsilon: f32) -> f32 {
    if value.abs() < epsilon {
        epsilon.copysign(value)
    } else {
        value
    }
}

fn wb_gains_for_temp_tint(cam_xyz: &[f32; 12], temp_k: f32, tint: f32) -> [f32; 3] {
    let (x, y) = planckian_locus_xy(temp_k);
    let y = clamp_denominator(y - tint * 0.0003, 1e-6);
    let sum_xyz = [x, y, 1.0 - x - y];
    let xyz = [sum_xyz[0] / y, 1.0, sum_xyz[2] / y];
    // cam_xyz_to_mat3 returns XYZ->camera directly (no inversion needed here, unlike
    // camera_to_working_space_matrix, which needs camera->XYZ instead).
    let xyz_to_cam = cam_xyz_to_mat3(cam_xyz);
    let camera_neutral = mat3_apply(xyz_to_cam, xyz);
    let g = clamp_denominator(camera_neutral[1], 1e-6);
    [
        g / clamp_denominator(camera_neutral[0], 1e-6),
        1.0,
        g / clamp_denominator(camera_neutral[2], 1e-6),
    ]
}

/// Resolves a stage's [`WbParams`] to camera-space gains: `temp_k: None` uses the frame's own
/// as-shot `cam_mul`, with `tint` applied as a green-channel multiplier (there's no chromaticity
/// to shift `tint`'s xy offset against without an explicit temperature); `Some(k)` overrides both
/// via [`wb_gains_for_temp_tint`], which applies `tint` as a perpendicular xy shift instead. Either
/// way, PV2012's Temp and Tint sliders stay independent -- a tint-only edit never requires an
/// explicit temp.
pub fn wb_gains_with_params(cam_mul: [f32; 4], cam_xyz: &[f32; 12], wb: &WbParams) -> [f32; 3] {
    match wb.temp_k {
        Some(k) => wb_gains_for_temp_tint(cam_xyz, k, wb.tint),
        None => {
            let mut gains = wb_gains(cam_mul);
            let tint_mult = (1.0 - wb.tint * 0.0004).max(1e-3);
            gains[1] *= tint_mult;
            gains
        }
    }
}

/// Bradford-free XYZ(D50) -> linear ProPhoto RGB, the standard published matrix (ProPhoto RGB's
/// own native white point is D50, so no chromatic-adaptation step is needed here -- a real
/// illuminant-dependent adaptation, e.g. for a strongly non-D50 as-shot white balance, is #42's
/// DCP-profile scope (its second PR), not this pipeline-proving slice's; #42's display/output
/// color management lives in `nicti-calico`, outside this crate).
pub const XYZ_D50_TO_PROPHOTO: Mat3 = [
    [1.3459433, -0.2556075, -0.0511118],
    [-0.5445989, 1.5081673, 0.0205351],
    [0.0000000, 0.0000000, 1.2118128],
];

/// The full camera-RGB -> working-space (linear ProPhoto) matrix, folding white balance (a
/// diagonal gain matrix -- as-shot, or a manual temp/tint override, see [`wb_gains_with_params`])
/// and the camera -> XYZ(D50) -> ProPhoto chain into one 3x3 -- linear operations compose, so this
/// is exactly equivalent to applying WB, then cam->XYZ, then XYZ->ProPhoto as three separate
/// steps, computed once per render on the CPU rather than on every pixel on the GPU.
pub fn camera_to_working_space_matrix(
    cam_mul: [f32; 4],
    cam_xyz: &[f32; 12],
    wb: &WbParams,
) -> Mat3 {
    let wb_mat = mat3_diag(wb_gains_with_params(cam_mul, cam_xyz, wb));
    // cam_xyz_to_mat3 returns XYZ->camera (see its own doc comment); invert to get camera->XYZ.
    let cam_to_xyz = mat3_invert(&cam_xyz_to_mat3(cam_xyz));
    mat3_mul(XYZ_D50_TO_PROPHOTO, mat3_mul(cam_to_xyz, wb_mat))
}

pub fn exposure_multiplier(stops: f32) -> f32 {
    2f32.powf(stops)
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Edge test on the perceptual (cube-root luma) range across the neighbourhood, with LightCraft's
/// smoothstep bounds. The defringe idea is adapted from storytold/lightcraft@265248c
/// `crates/pipeline/src/optics.rs` (and `finish.rs`), Copyright (c) 2026 ArtCraft Team and the
/// LightCraft contributors, MIT OR Apache-2.0 (see `docs/licensing.md`); re-expressed here in HSV hue
/// with a compass-tap edge test rather than ported line for line.
const DEFRINGE_EDGE: (f32, f32) = (0.08, 0.25);
/// HSV saturation below which a pixel is too grey to carry a visible fringe.
const DEFRINGE_SAT: (f32, f32) = (0.05, 0.20);
/// Soft shoulder on both ends of a hue window, in degrees.
const DEFRINGE_SHOULDER_DEG: f32 = 8.0;
/// The 0..1 hue sliders span these HSV hue ranges (degrees, 120 wide each): purple/magenta 240..360,
/// green 60..180. LightCraft gates in OkLab hue; HSV hue of the linear working space is what the
/// live shader already has, so the same windows are expressed there. **Not yet tuned on real
/// photos** (#428's exit asks for it).
const DEFRINGE_PURPLE_BASE: f32 = 240.0;
const DEFRINGE_GREEN_BASE: f32 = 60.0;
const DEFRINGE_HUE_SPAN: f32 = 120.0;

/// Pixels between the centre and each of the 8 compass-direction taps the edge test reads, for a
/// frame whose long edge is `long_edge` px: 2 px at 4000 px across, scaled with resolution so the
/// same *content* is considered an edge at any size. Mirrored in `live_suffix.wgsl`.
pub fn defringe_radius(long_edge: u32) -> i32 {
    // `floor(x + 0.5)`, not `round`: WGSL rounds halves to even, Rust away from zero.
    ((2.0 * long_edge as f32 / 4000.0 + 0.5).floor()).clamp(1.0, 6.0) as i32
}

/// The 8 tap offsets (in units of [`defringe_radius`]) both implementations read.
pub const DEFRINGE_TAPS: [(i32, i32); 8] = [
    (-1, -1),
    (0, -1),
    (1, -1),
    (-1, 0),
    (1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
];

/// DNG-spec HSV hue (degrees) and saturation of a linear working-space pixel -- the same math as
/// `live_suffix.wgsl`'s `dcp_rgb_to_hsv`.
fn hue_sat(rgb: [f32; 3]) -> (f32, f32) {
    let max_c = rgb[0].max(rgb[1]).max(rgb[2]);
    let min_c = rgb[0].min(rgb[1]).min(rgb[2]);
    let delta = max_c - min_c;
    let mut h = 0.0;
    if delta > 1e-6 {
        h = if max_c == rgb[0] {
            60.0 * (((rgb[1] - rgb[2]) / delta) % 6.0)
        } else if max_c == rgb[1] {
            60.0 * ((rgb[2] - rgb[0]) / delta + 2.0)
        } else {
            60.0 * ((rgb[0] - rgb[1]) / delta + 4.0)
        };
    }
    if h < 0.0 {
        h += 360.0;
    }
    let s = if max_c > 0.0 { delta / max_c } else { 0.0 };
    (h, s)
}

/// 1 inside `[lo, hi]` degrees, falling to 0 over the shoulder on each side; also true across the
/// 360/0 seam (a window ending at 360 still catches hue 4).
fn hue_window(h: f32, lo: f32, hi: f32) -> f32 {
    let at = |h: f32| {
        smoothstep(lo - DEFRINGE_SHOULDER_DEG, lo, h)
            * (1.0 - smoothstep(hi, hi + DEFRINGE_SHOULDER_DEG, h))
    };
    at(h).max(at(h + 360.0))
}

fn perceptual_l(rgb: [f32; 3]) -> f32 {
    (0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2])
        .max(0.0)
        .cbrt()
}

/// Defringe (#428): desaturates `rgb` (linear working space, as it leaves the camera->working
/// matrix) toward its luma when it is a saturated purple/green pixel next to a strong luminance
/// edge. `taps` are the 8 neighbours at [`defringe_radius`] in [`DEFRINGE_TAPS`] order, in the
/// same space. CPU twin of `live_suffix.wgsl`'s `apply_defringe`.
pub fn defringe_pixel(rgb: [f32; 3], taps: &[[f32; 3]; 8], p: &DefringeParams) -> [f32; 3] {
    if p.is_noop() {
        return rgb;
    }
    let (hue, sat) = hue_sat(rgb);
    // Linear in the normalized amount (LRC 0..20): every slider position does something, so an
    // imported LRC amount of 8 and of 20 stay distinguishable. (LightCraft saturated at 8 of 20.)
    let strength = |amount: f32| amount.clamp(0.0, 1.0);
    let window = |base: f32, lo: f32, hi: f32| {
        hue_window(
            hue,
            base + DEFRINGE_HUE_SPAN * lo,
            base + DEFRINGE_HUE_SPAN * hi,
        )
    };
    let purple =
        strength(p.purple_amount) * window(DEFRINGE_PURPLE_BASE, p.purple_hue_lo, p.purple_hue_hi);
    let green =
        strength(p.green_amount) * window(DEFRINGE_GREEN_BASE, p.green_hue_lo, p.green_hue_hi);
    let gate = purple.max(green) * smoothstep(DEFRINGE_SAT.0, DEFRINGE_SAT.1, sat);
    if gate <= 0.0 {
        return rgb;
    }
    let l = perceptual_l(rgb);
    let (mut lo, mut hi) = (l, l);
    for t in taps {
        let v = perceptual_l(*t);
        lo = lo.min(v);
        hi = hi.max(v);
    }
    let k = gate * smoothstep(DEFRINGE_EDGE.0, DEFRINGE_EDGE.1, hi - lo);
    let luma = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    rgb.map(|c| luma + (c - luma) * (1.0 - k))
}

/// Basic-panel tone controls in a rough perceptual (cube-root) space: whites/blacks remap the
/// endpoints, contrast pivots around mid-grey, and highlights/shadows apply a luminance-weighted
/// additive shift (smooth masks favoring bright/dark pixels respectively). Every field at 0.0 is
/// a no-op. This is a v1, deliberately *global* approximation of LRC's own locally-adaptive
/// highlights/shadows (a real per-pixel-neighborhood version is a documented follow-up, not this
/// ticket's scope) -- a rendered-image comparison against real LRC exports is #46's own follow-up
/// once #202's reference-machine run exists. Operates on non-negative linear values; the final
/// cube never goes negative since `shifted` is clamped first.
pub fn apply_tone(rgb: [f32; 3], tone: &ToneParams) -> [f32; 3] {
    // Positive `whites` moves the white point *down* (values reach 1.0 sooner -> brighter
    // highlights); negative `blacks` moves the black point *up* (values reach 0.0 sooner ->
    // crushed, darker shadows) -- matching LRC's own Whites-right-brightens/Blacks-left-darkens
    // slider convention, not a naive same-sign offset.
    let white_point = 1.0 - tone.whites * 0.3;
    let black_point = -tone.blacks * 0.3;
    let range = (white_point - black_point).max(1e-4);

    let perceptual = rgb.map(|c| c.max(0.0).cbrt());
    let remapped = perceptual.map(|p| (p - black_point) / range);
    let contrasted = remapped.map(|p| (p - 0.5) * (1.0 + tone.contrast) + 0.5);

    let luma = 0.2126 * contrasted[0] + 0.7152 * contrasted[1] + 0.0722 * contrasted[2];
    let hi_w = smoothstep(0.35, 0.9, luma);
    let sh_w = 1.0 - smoothstep(0.1, 0.65, luma);
    let shifted =
        contrasted.map(|p| p + tone.highlights * 0.25 * hi_w + tone.shadows * 0.25 * sh_w);

    shifted.map(|p| p.max(0.0).powi(3))
}

/// Fixed x-positions of the Tone Curve's 4 region-slider control points -- see
/// [`ToneCurveParams`]'s own doc comment for why these are fixed rather than user-adjustable.
const TONE_CURVE_X: [f32; 4] = [0.0, 0.25, 0.75, 1.0];

/// A 256-entry lookup table sampling a monotone cubic (Fritsch-Carlson/PCHIP) spline through the
/// Tone Curve's 4 control points -- built once per render (not per pixel) on the CPU, then either
/// looked up directly ([`apply_tone_curve`]'s CPU reference) or uploaded into the live-suffix
/// uniform buffer for the GPU kernel to sample. Each region slider moves its own control point's
/// y-value by up to +/-0.3 (matching [`apply_tone`]'s own 0.3 magnitude for whites/blacks), then
/// clamps to `0.0..=1.0` -- PCHIP itself does not enforce monotonicity when the (possibly
/// clamped) control points aren't themselves monotonic, e.g. at extreme, opposing slider values;
/// that's an accepted v1 edge case, not a correctness bug for any single slider in isolation.
pub fn build_tone_curve_lut(curve: &ToneCurveParams) -> [f32; 256] {
    let shifts = [curve.shadows, curve.darks, curve.lights, curve.highlights];
    let xs = TONE_CURVE_X;
    let ys: [f32; 4] = std::array::from_fn(|i| (xs[i] + shifts[i] * 0.3).clamp(0.0, 1.0));

    // Fritsch-Carlson tangents: 0 at a local extremum (a sign change or a flat segment) rather
    // than the naive average, which is what keeps a monotone *input* producing a monotone
    // *output* (Hyman/PCHIP's whole point).
    let h: [f32; 3] = std::array::from_fn(|i| xs[i + 1] - xs[i]);
    let delta: [f32; 3] = std::array::from_fn(|i| (ys[i + 1] - ys[i]) / h[i]);
    let mut m = [0.0f32; 4];
    m[0] = delta[0];
    m[3] = delta[2];
    for i in 1..3 {
        let (d0, d1) = (delta[i - 1], delta[i]);
        m[i] = if d0 == 0.0 || d1 == 0.0 || d0.signum() != d1.signum() {
            0.0
        } else {
            let (w1, w2) = (2.0 * h[i] + h[i - 1], h[i] + 2.0 * h[i - 1]);
            (w1 + w2) / (w1 / d0 + w2 / d1)
        };
    }

    let mut lut = [0.0f32; 256];
    for (i, entry) in lut.iter_mut().enumerate() {
        let x = i as f32 / 255.0;
        // Find the segment: xs is fixed and sorted, so a linear scan over 3 segments is fine.
        let seg = if x < xs[1] {
            0
        } else if x < xs[2] {
            1
        } else {
            2
        };
        let hseg = h[seg];
        let t = ((x - xs[seg]) / hseg).clamp(0.0, 1.0);
        let (t2, t3) = (t * t, t * t * t);
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        *entry =
            (h00 * ys[seg] + h10 * hseg * m[seg] + h01 * ys[seg + 1] + h11 * hseg * m[seg + 1])
                .clamp(0.0, 1.0);
    }
    lut
}

/// Applies a 256-entry tone-curve LUT (see [`build_tone_curve_lut`]) per channel, in the same
/// cube-root perceptual space [`apply_tone`] already uses -- LRC's own Tone Curve panel is applied
/// per-channel identically, not against a single luma value.
pub fn apply_tone_curve(rgb: [f32; 3], lut: &[f32; 256]) -> [f32; 3] {
    rgb.map(|c| {
        let perceptual = c.max(0.0).cbrt().clamp(0.0, 1.0);
        let pos = perceptual * 255.0;
        let i0 = (pos.floor() as usize).min(254);
        let frac = pos - i0 as f32;
        let looked_up = lut[i0] * (1.0 - frac) + lut[i0 + 1] * frac;
        looked_up.max(0.0).powi(3)
    })
}

/// Per-channel point-curve LUTs (#432): rows are R, G, B, each 256 entries over the cube-root
/// perceptual axis, with the master curve already composed in (`channel(master(x))`). `None` when
/// every curve is the identity, so the shader can skip the stage and leave pixels bit-identical.
/// Built from [`PointCurveParams::sanitized`] points with the same Fritsch-Carlson monotone spline
/// the DCP profile curve uses (`nicti_calico::tonecurve::ToneCurve`).
pub fn build_point_curve_luts(params: &PointCurveParams) -> Option<[[f32; 256]; 3]> {
    use nicti_calico::tonecurve::ToneCurve;
    let s = params.sanitized();
    if s.master.is_empty() && s.red.is_empty() && s.green.is_empty() && s.blue.is_empty() {
        return None;
    }
    let curve = |pts: &[[f32; 2]]| -> Option<ToneCurve> {
        (!pts.is_empty()).then(|| {
            let pts: Vec<(f64, f64)> = pts
                .iter()
                .map(|p| (f64::from(p[0]), f64::from(p[1])))
                .collect();
            ToneCurve::new(&pts)
        })
    };
    let master = curve(&s.master);
    let channels = [curve(&s.red), curve(&s.green), curve(&s.blue)];
    Some(std::array::from_fn(|c| {
        std::array::from_fn(|i| {
            let x = f64::from(i as f32 / 255.0);
            let m = master.as_ref().map_or(x, |m| m.eval(x));
            let y = channels[c].as_ref().map_or(m, |ch| ch.eval(m));
            y.clamp(0.0, 1.0) as f32
        })
    }))
}

/// Applies [`build_point_curve_luts`]'s tables per channel in the cube-root perceptual space,
/// mirroring [`apply_tone_curve`] (linear interpolation between adjacent entries). CPU twin of
/// `live_suffix.wgsl`'s `apply_point_curve`.
pub fn apply_point_curve(rgb: [f32; 3], luts: &[[f32; 256]; 3]) -> [f32; 3] {
    std::array::from_fn(|c| {
        let perceptual = rgb[c].max(0.0).cbrt().clamp(0.0, 1.0);
        let pos = perceptual * 255.0;
        let i0 = (pos.floor() as usize).min(254);
        let frac = pos - i0 as f32;
        let looked_up = luts[c][i0] * (1.0 - frac) + luts[c][i0 + 1] * frac;
        looked_up.max(0.0).powi(3)
    })
}

/// Hue in degrees (`0.0..360.0`), from the standard "which channel is max" formula -- built only
/// from `(max, min, delta)` ratios, so it stays well-defined for the unbounded-above-1.0 linear
/// working-space values this module works in (only division by `delta` risks instability, guarded
/// by the caller checking `delta` first). Returns `0.0` for an achromatic pixel (`delta == 0.0`);
/// callers must check for that case themselves since hue is undefined there, not `0.0` by
/// meaning.
fn rgb_hue_degrees(rgb: [f32; 3], max: f32, delta: f32) -> f32 {
    let [r, g, b] = rgb;
    let raw = if max == r {
        ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    (raw * 60.0).rem_euclid(360.0)
}

/// Smallest signed angular distance from `hue` to `center`, in `-180.0..=180.0` degrees.
fn hue_delta_degrees(hue: f32, center: f32) -> f32 {
    let raw = (hue - center).rem_euclid(360.0);
    if raw > 180.0 {
        raw - 360.0
    } else {
        raw
    }
}

/// This band's membership weight for a pixel at `hue` degrees -- a raised-cosine (Hann) window
/// spanning +/-45 degrees around the band's own center, so adjacent bands (45 degrees apart, per
/// [`HslParams`]'s doc comment) cross over at weight 0.5 exactly at their shared midpoint, with no
/// hard edges between bands.
fn hsl_band_weight(hue: f32, band_index: usize) -> f32 {
    let center = band_index as f32 * 45.0;
    let d = hue_delta_degrees(hue, center).abs();
    if d >= 45.0 {
        0.0
    } else {
        0.5 * (1.0 + (std::f32::consts::PI * d / 45.0).cos())
    }
}

/// HSL panel, 8-band Hue/Saturation/Luminance adjustment. A v1 approximation of LRC's own HSL
/// panel: hue/saturation are adjusted in HSV space (`v = max` handles this crate's unbounded-
/// above-1.0 linear working-space values the way canonical HSL's `l = (max+min)/2` cannot --
/// `(1 - |2l-1|)`'s denominator goes negative for `l > 1`), while the luminance shift is a
/// separate additive step in the same cube-root perceptual space [`apply_tone`] uses. An
/// achromatic pixel (`max == min`, no defined hue) passes through unchanged -- correct, since
/// every band's saturation is already 0 there; there's no per-pixel hue test to skip this
/// function itself. [`HslParams::is_noop`] exists for a caller that wants to skip the whole HSL
/// pass at the params level (this stage is fused into the same per-pixel dispatch every other
/// live stage shares, so nothing here currently calls it for that purpose -- see the sibling
/// [`crate::coat::SharpenParams::is_noop`]/[`crate::coat::NoiseReductionParams::is_noop`] for the
/// stage that actually does skip work based on it, `LiveSuffixKernel::encode`'s fast path).
pub fn apply_hsl(rgb: [f32; 3], hsl: &HslParams) -> [f32; 3] {
    let max = rgb[0].max(rgb[1]).max(rgb[2]);
    let min = rgb[0].min(rgb[1]).min(rgb[2]);
    let delta = max - min;
    if delta <= 1e-6 || max <= 0.0 {
        return rgb;
    }

    let hue = rgb_hue_degrees(rgb, max, delta);
    let weights: [f32; 8] = std::array::from_fn(|i| hsl_band_weight(hue, i));
    let hue_shift: f32 = weights
        .iter()
        .zip(hsl.bands.iter())
        .map(|(w, b)| w * b.hue)
        .sum::<f32>()
        * 30.0;
    let sat_shift: f32 = weights
        .iter()
        .zip(hsl.bands.iter())
        .map(|(w, b)| w * b.saturation)
        .sum();
    let luma_shift: f32 = weights
        .iter()
        .zip(hsl.bands.iter())
        .map(|(w, b)| w * b.luminance)
        .sum::<f32>()
        * 0.3;

    let sat = (delta / max).clamp(0.0, 1.0);
    let new_hue = (hue + hue_shift).rem_euclid(360.0);
    let new_sat = (sat * (1.0 + sat_shift)).max(0.0);
    let hue_rotated = hsv_to_rgb(new_hue, new_sat, max);

    let perceptual = hue_rotated.map(|c| c.max(0.0).cbrt());
    perceptual.map(|p| (p + luma_shift).max(0.0).powi(3))
}

/// Standard HSV -> RGB, sector formula. `v` is not assumed to be in `0.0..=1.0` -- it's just
/// carried through as an overall scale, matching this module's unbounded-above-1.0 working-space
/// convention.
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let c = v * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - (h_prime.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = if h_prime < 1.0 {
        (c, x, 0.0)
    } else if h_prime < 2.0 {
        (x, c, 0.0)
    } else if h_prime < 3.0 {
        (0.0, c, x)
    } else if h_prime < 4.0 {
        (0.0, x, c)
    } else if h_prime < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = v - c;
    [r1 + m, g1 + m, b1 + m]
}

/// Luma-preserving saturation boost, weighted more heavily on already-low-saturation pixels (the
/// conventional definition of "vibrance" vs. a flat "saturation" boost). `vibrance` of 0.0 is a
/// no-op. Luma uses Rec.709 weights as a working approximation in ProPhoto space, not a
/// colorimetrically exact ProPhoto luminance -- adequate for a saturation-boost weighting, not
/// claimed as radiometrically precise.
pub fn apply_vibrance(rgb: [f32; 3], vibrance: f32) -> [f32; 3] {
    let max = rgb[0].max(rgb[1]).max(rgb[2]);
    let min = rgb[0].min(rgb[1]).min(rgb[2]);
    let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
    let boost = vibrance * (1.0 - sat);
    let luma = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    rgb.map(|c| luma + (c - luma) * (1.0 + boost))
}

pub fn srgb_oetf(linear: f32) -> f32 {
    let c = linear.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coat::HslBand;

    #[test]
    fn mat3_invert_of_identity_is_identity() {
        assert_eq!(mat3_invert(&mat3_identity()), mat3_identity());
    }

    #[test]
    fn mat3_invert_round_trips() {
        let m = XYZ_D50_TO_PROPHOTO;
        let round_tripped = mat3_mul(mat3_invert(&m), m);
        for (r, row) in round_tripped.iter().enumerate() {
            for (c, &actual) in row.iter().enumerate() {
                let expected = if r == c { 1.0 } else { 0.0 };
                assert!(
                    (actual - expected).abs() < 1e-4,
                    "[{r}][{c}]: {actual} vs {expected}"
                );
            }
        }
    }

    #[test]
    fn camera_to_working_space_matrix_inverts_cam_xyz_direction() {
        // cam_xyz is XYZ->camera (see cam_xyz_to_mat3's doc comment); a non-identity, invertible
        // cam_xyz must be inverted, not used as-is, to reach camera->XYZ. This is a regression
        // test for a real bug: using cam_xyz un-inverted produced badly wrong colors (a strong
        // green cast) against a real NEF, caught in #45 PR4's real-hardware verification pass.
        let cam_mul = [1.0, 1.0, 1.0, 1.0];
        let cam_xyz = [
            0.5, 0.1, 0.0, // XYZ->camera R row
            0.0, 0.6, 0.1, // XYZ->camera G row
            0.1, 0.0, 0.7, // XYZ->camera B row
            0.0, 0.0, 0.0, // unused G2 row
        ];
        let m = camera_to_working_space_matrix(cam_mul, &cam_xyz, &WbParams::default());
        let expected = mat3_mul(XYZ_D50_TO_PROPHOTO, mat3_invert(&cam_xyz_to_mat3(&cam_xyz)));
        for r in 0..3 {
            for c in 0..3 {
                assert!((m[r][c] - expected[r][c]).abs() < 1e-6, "[{r}][{c}]");
            }
        }
    }

    #[test]
    fn mat3_mul_is_associative_with_identity() {
        let m = XYZ_D50_TO_PROPHOTO;
        assert_eq!(mat3_mul(m, mat3_identity()), m);
        assert_eq!(mat3_mul(mat3_identity(), m), m);
    }

    #[test]
    fn wb_gains_normalizes_green_to_one() {
        let gains = wb_gains([2.0, 1.0, 1.5, 1.0]);
        assert_eq!(gains, [2.0, 1.0, 1.5]);
    }

    #[test]
    fn wb_gains_handles_a_zero_green_multiplier_without_dividing_by_zero() {
        let gains = wb_gains([2.0, 0.0, 1.5, 0.0]);
        assert!(gains.iter().all(|g| g.is_finite()));
    }

    #[test]
    fn camera_to_working_space_matrix_is_identity_when_wb_and_cam_xyz_are_both_identity() {
        // An identity cam_xyz inverts to itself, so this doesn't exercise the inversion direction
        // (see camera_to_working_space_matrix_inverts_cam_xyz_direction for that) -- it only
        // pins the outer XYZ_D50_TO_PROPHOTO composition when WB and cam_xyz are both no-ops.
        let cam_mul = [1.0, 1.0, 1.0, 1.0];
        let identity_cam_xyz = [
            1.0, 0.0, 0.0, // R row
            0.0, 1.0, 0.0, // G row
            0.0, 0.0, 1.0, // B row
            0.0, 0.0, 0.0, // unused G2 row
        ];
        let m = camera_to_working_space_matrix(cam_mul, &identity_cam_xyz, &WbParams::default());
        assert_eq!(m, XYZ_D50_TO_PROPHOTO);
    }

    #[test]
    fn apply_tone_with_all_zero_params_is_a_near_identity() {
        let rgb = [0.2, 0.5, 0.8];
        let out = apply_tone(rgb, &ToneParams::default());
        for (a, b) in rgb.iter().zip(out.iter()) {
            assert!((a - b).abs() < 1e-4, "{a} vs {b}");
        }
    }

    #[test]
    fn apply_tone_positive_contrast_increases_spread_from_pivot() {
        let tone = ToneParams {
            contrast: 0.5,
            ..Default::default()
        };
        let low = apply_tone([0.1, 0.1, 0.1], &tone)[0];
        let high = apply_tone([0.9, 0.9, 0.9], &tone)[0];
        assert!(low < 0.1, "low value should get darker: {low}");
        assert!(high > 0.9, "high value should get brighter: {high}");
    }

    #[test]
    fn apply_tone_whites_brightens_a_near_white_pixel() {
        let base = apply_tone([0.9, 0.9, 0.9], &ToneParams::default())[0];
        let whites = apply_tone(
            [0.9, 0.9, 0.9],
            &ToneParams {
                whites: 0.5,
                ..Default::default()
            },
        )[0];
        assert!(
            whites > base,
            "positive whites should brighten: {whites} vs {base}"
        );
    }

    #[test]
    fn apply_tone_blacks_darkens_a_near_black_pixel() {
        let base = apply_tone([0.05, 0.05, 0.05], &ToneParams::default())[0];
        let blacks = apply_tone(
            [0.05, 0.05, 0.05],
            &ToneParams {
                blacks: -0.5,
                ..Default::default()
            },
        )[0];
        assert!(
            blacks < base,
            "negative blacks should darken: {blacks} vs {base}"
        );
    }

    #[test]
    fn apply_tone_highlights_only_affects_bright_pixels() {
        let tone = ToneParams {
            highlights: -0.8,
            ..Default::default()
        };
        let dark_base = apply_tone([0.05, 0.05, 0.05], &ToneParams::default())[0];
        let dark_shifted = apply_tone([0.05, 0.05, 0.05], &tone)[0];
        assert!(
            (dark_base - dark_shifted).abs() < 1e-3,
            "a dark pixel should be nearly unaffected by highlights: {dark_base} vs {dark_shifted}"
        );
        let bright_base = apply_tone([0.95, 0.95, 0.95], &ToneParams::default())[0];
        let bright_shifted = apply_tone([0.95, 0.95, 0.95], &tone)[0];
        assert!(
            bright_shifted < bright_base,
            "negative highlights should darken a bright pixel: {bright_shifted} vs {bright_base}"
        );
    }

    #[test]
    fn wb_gains_with_params_as_shot_matches_wb_gains_when_tint_is_zero() {
        let cam_mul = [2.0, 1.0, 1.5, 1.0];
        let gains = wb_gains_with_params(
            cam_mul,
            &[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
            &WbParams::default(),
        );
        assert_eq!(gains, wb_gains(cam_mul));
    }

    #[test]
    fn wb_gains_with_params_temp_override_produces_finite_positive_gains() {
        let cam_xyz = [
            0.6, 0.2, 0.1, 0.15, 0.75, 0.1, 0.05, 0.15, 0.9, 0.0, 0.0, 0.0,
        ];
        for temp_k in [2500.0, 4200.0, 5500.0, 6500.0, 9000.0] {
            let gains = wb_gains_with_params(
                [1.0, 1.0, 1.0, 1.0],
                &cam_xyz,
                &WbParams {
                    temp_k: Some(temp_k),
                    tint: 0.0,
                },
            );
            assert!(
                gains.iter().all(|g| g.is_finite() && *g > 0.0),
                "temp_k={temp_k}: gains={gains:?}"
            );
        }
    }

    #[test]
    fn clamp_denominator_preserves_sign_instead_of_flipping_it() {
        // Regression test: `.max(epsilon)` alone would turn -0.5 into epsilon (a small positive
        // number), not -epsilon -- silently flipping the sign of whatever divides by it.
        assert_eq!(clamp_denominator(-0.5, 1e-6), -0.5);
        assert_eq!(clamp_denominator(0.5, 1e-6), 0.5);
        assert_eq!(clamp_denominator(-1e-9, 1e-6), -1e-6);
        assert_eq!(clamp_denominator(1e-9, 1e-6), 1e-6);
        assert_eq!(clamp_denominator(0.0, 1e-6), 1e-6);
    }

    #[test]
    fn wb_gains_for_temp_tint_does_not_flip_sign_on_a_negative_camera_response() {
        // Regression test for a real bug: a camera matrix with a negative off-diagonal entry can
        // legitimately put `camera_neutral`'s R or B channel below zero at some chromaticity --
        // `.max(epsilon)` alone would silently floor that up to a tiny *positive* number instead
        // of clamping its magnitude, producing a wildly wrong-signed gain. This cam_xyz is
        // engineered so the R-channel response at daylight-ish chromaticities goes negative.
        let cam_xyz = [
            -0.6, 0.2, 0.1, 0.15, 0.75, 0.1, 0.05, 0.15, 0.9, 0.0, 0.0, 0.0,
        ];
        let gains = wb_gains_for_temp_tint(&cam_xyz, 5500.0, 0.0);
        assert!(
            gains.iter().all(|g| g.is_finite()),
            "gains must stay finite: {gains:?}"
        );
        // The old `.max(1e-6)` bug produced a gain with |g[0]| in the hundreds of thousands
        // (dividing by a ~1e-6-floored near-zero denominator); a magnitude-clamped, sign-
        // preserving denominator keeps the gain in a plausible WB range instead.
        assert!(
            gains[0].abs() < 100.0,
            "gain magnitude should stay plausible, not blow up from a sign-flipped clamp: {gains:?}"
        );
    }

    #[test]
    fn wb_gains_for_temp_tint_handles_a_tint_extreme_enough_to_push_y_toward_zero() {
        // At an extreme (out-of-slider-range) tint, `y` can approach zero -- clamp_denominator
        // must keep the xyz->camera_neutral division finite rather than blowing up.
        let cam_xyz = [
            0.6, 0.2, 0.1, 0.15, 0.75, 0.1, 0.05, 0.15, 0.9, 0.0, 0.0, 0.0,
        ];
        let gains = wb_gains_for_temp_tint(&cam_xyz, 5500.0, 1_000_000.0);
        assert!(
            gains.iter().all(|g| g.is_finite()),
            "gains must stay finite even for an extreme tint: {gains:?}"
        );
    }

    #[test]
    fn wb_gains_with_params_tint_shifts_green_gain_in_the_as_shot_path() {
        let cam_mul = [1.0, 1.0, 1.0, 1.0];
        let identity_cam_xyz = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0];
        let neutral = wb_gains_with_params(cam_mul, &identity_cam_xyz, &WbParams::default());
        let tinted = wb_gains_with_params(
            cam_mul,
            &identity_cam_xyz,
            &WbParams {
                temp_k: None,
                tint: 50.0,
            },
        );
        assert_ne!(neutral[1], tinted[1]);
        assert_eq!(neutral[0], tinted[0]);
        assert_eq!(neutral[2], tinted[2]);
    }

    #[test]
    fn apply_vibrance_with_zero_vibrance_is_identity() {
        let rgb = [0.3, 0.6, 0.1];
        let out = apply_vibrance(rgb, 0.0);
        for (a, b) in rgb.iter().zip(out.iter()) {
            assert!((a - b).abs() < 1e-5, "{a} vs {b}");
        }
    }

    #[test]
    fn apply_vibrance_boosts_a_low_saturation_pixel_more_than_a_high_saturation_one() {
        let low_sat = [0.5, 0.52, 0.48]; // near-gray
        let high_sat = [0.9, 0.1, 0.1]; // already very saturated
        let low_out = apply_vibrance(low_sat, 0.5);
        let high_out = apply_vibrance(high_sat, 0.5);
        let low_spread =
            low_out[0].max(low_out[1]).max(low_out[2]) - low_out[0].min(low_out[1]).min(low_out[2]);
        let low_spread_before = 0.52 - 0.48;
        let high_spread = high_out[0].max(high_out[1]).max(high_out[2])
            - high_out[0].min(high_out[1]).min(high_out[2]);
        let high_spread_before = 0.9 - 0.1;
        let low_growth = low_spread / low_spread_before;
        let high_growth = high_spread / high_spread_before;
        assert!(
            low_growth > high_growth,
            "low-saturation pixel should gain relatively more spread: {low_growth} vs {high_growth}"
        );
    }

    #[test]
    fn tone_curve_lut_is_identity_at_all_zero_params() {
        let lut = build_tone_curve_lut(&ToneCurveParams::default());
        for (i, &entry) in lut.iter().enumerate() {
            let x = i as f32 / 255.0;
            assert!(
                (entry - x).abs() < 1e-3,
                "lut[{i}]={entry} should be near-identity {x}",
            );
        }
    }

    #[test]
    fn tone_curve_lut_endpoints_are_pinned_regardless_of_interior_sliders() {
        let curve = ToneCurveParams {
            darks: -0.8,
            lights: 0.8,
            ..Default::default()
        };
        let lut = build_tone_curve_lut(&curve);
        assert!((lut[0] - 0.0).abs() < 1e-3);
        assert!((lut[255] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn tone_curve_lut_shadows_slider_moves_only_the_low_end() {
        let base = build_tone_curve_lut(&ToneCurveParams::default());
        let lifted = build_tone_curve_lut(&ToneCurveParams {
            shadows: 0.5,
            ..Default::default()
        });
        assert!(
            lifted[0] > base[0],
            "positive shadows should lift the black point: {} vs {}",
            lifted[0],
            base[0]
        );
        assert!(
            (lifted[255] - base[255]).abs() < 1e-3,
            "shadows should barely move the white point: {} vs {}",
            lifted[255],
            base[255]
        );
    }

    #[test]
    fn apply_tone_curve_with_identity_lut_is_near_identity() {
        let lut = build_tone_curve_lut(&ToneCurveParams::default());
        let rgb = [0.2, 0.5, 0.8];
        let out = apply_tone_curve(rgb, &lut);
        for (a, b) in rgb.iter().zip(out.iter()) {
            assert!((a - b).abs() < 1e-2, "{a} vs {b}");
        }
    }

    fn dark() -> [f32; 3] {
        [0.05, 0.05, 0.05]
    }
    fn bright() -> [f32; 3] {
        [0.8, 0.8, 0.8]
    }
    fn purple() -> [f32; 3] {
        [0.45, 0.2, 0.55]
    }
    fn green() -> [f32; 3] {
        [0.25, 0.6, 0.2]
    }
    /// Taps straddling an edge: some dark, some bright.
    fn edge_taps() -> [[f32; 3]; 8] {
        [
            dark(),
            dark(),
            dark(),
            dark(),
            bright(),
            bright(),
            bright(),
            bright(),
        ]
    }
    fn flat_taps(c: [f32; 3]) -> [[f32; 3]; 8] {
        [c; 8]
    }
    fn chroma(c: [f32; 3]) -> f32 {
        let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        c.iter().map(|v| (v - l).abs()).sum()
    }

    #[test]
    fn defringe_desaturates_purple_at_an_edge_but_not_a_flat_purple_area() {
        let p = DefringeParams {
            purple_amount: 1.0,
            ..Default::default()
        };
        let fringed = defringe_pixel(purple(), &edge_taps(), &p);
        assert!(chroma(fringed) < 0.1 * chroma(purple()), "{fringed:?}");
        // The same colour with no edge around it is a real purple object: untouched.
        assert_eq!(defringe_pixel(purple(), &flat_taps(purple()), &p), purple());
    }

    #[test]
    fn defringe_green_channel_works_and_the_channels_do_not_cross() {
        let green_only = DefringeParams {
            green_amount: 1.0,
            ..Default::default()
        };
        assert!(chroma(defringe_pixel(green(), &edge_taps(), &green_only)) < 0.1 * chroma(green()));
        // A purple pixel is outside the green window.
        assert_eq!(
            defringe_pixel(purple(), &edge_taps(), &green_only),
            purple()
        );
        let purple_only = DefringeParams {
            purple_amount: 1.0,
            ..Default::default()
        };
        assert_eq!(defringe_pixel(green(), &edge_taps(), &purple_only), green());
    }

    #[test]
    fn defringe_leaves_greys_and_other_hues_alone() {
        let p = DefringeParams {
            purple_amount: 1.0,
            green_amount: 1.0,
            ..Default::default()
        };
        // Neutral edge pixel, and an orange one (hue ~30 degrees: in neither window).
        assert_eq!(defringe_pixel(bright(), &edge_taps(), &p), bright());
        let orange = [0.7, 0.4, 0.1];
        assert_eq!(defringe_pixel(orange, &edge_taps(), &p), orange);
        assert_eq!(
            defringe_pixel(purple(), &edge_taps(), &DefringeParams::default()),
            purple(),
            "amount 0 is the identity"
        );
    }

    #[test]
    fn defringe_strength_is_linear_across_the_whole_slider() {
        let at = |amount: f32| {
            chroma(defringe_pixel(
                purple(),
                &edge_taps(),
                &DefringeParams {
                    purple_amount: amount,
                    ..Default::default()
                },
            ))
        };
        // No dead range: each step of the slider removes more chroma, all the way to the end.
        let steps: Vec<f32> = [0.2, 0.4, 0.6, 0.8, 1.0].map(at).to_vec();
        assert!(steps.windows(2).all(|w| w[1] < w[0]), "{steps:?}");
        assert!(
            steps[4] < 1e-5,
            "full amount removes all the chroma: {steps:?}"
        );
    }

    #[test]
    fn defringe_hue_window_follows_the_sliders_and_wraps_the_seam() {
        // A magenta-red pixel at hue ~350: inside the default purple window? 240 + 120*[0.3,0.7]
        // = 276..324 -> no. Widening the window to its top end (hi = 1.0 -> 360) catches it, via
        // the seam wrap.
        let magenta = [0.6, 0.15, 0.2];
        let (h, _) = hue_sat(magenta);
        assert!(h > 330.0 && h < 360.0, "hue {h}");
        let narrow = DefringeParams {
            purple_amount: 1.0,
            ..Default::default()
        };
        assert_eq!(defringe_pixel(magenta, &edge_taps(), &narrow), magenta);
        let wide = DefringeParams {
            purple_amount: 1.0,
            purple_hue_hi: 1.0,
            ..Default::default()
        };
        assert!(chroma(defringe_pixel(magenta, &edge_taps(), &wide)) < chroma(magenta));
        // Seam: a window ending at 360 still catches hue 1 (the far side of the wrap).
        assert!(hue_window(1.0, 300.0, 360.0) > 0.9);
    }

    #[test]
    fn defringe_radius_scales_with_resolution_and_is_bounded() {
        assert_eq!(defringe_radius(100), 1);
        assert_eq!(defringe_radius(4000), 2);
        assert_eq!(defringe_radius(8256), 4);
        assert_eq!(defringe_radius(1_000_000), 6);
        // The half-way case rounds the same way the shader's floor(x + 0.5) does.
        assert_eq!(defringe_radius(5000), 3);
    }

    #[test]
    fn apply_hsl_with_all_zero_params_is_identity() {
        let rgb = [0.6, 0.2, 0.3];
        let out = apply_hsl(rgb, &HslParams::default());
        for (a, b) in rgb.iter().zip(out.iter()) {
            assert!((a - b).abs() < 1e-4, "{a} vs {b}");
        }
    }

    #[test]
    fn apply_hsl_passes_through_an_achromatic_pixel_unchanged() {
        let rgb = [0.4, 0.4, 0.4];
        let mut hsl = HslParams::default();
        hsl.bands[0].saturation = 1.0; // red band, maxed
        let out = apply_hsl(rgb, &hsl);
        assert_eq!(out, rgb);
    }

    #[test]
    fn apply_hsl_red_band_change_leaves_pure_blue_untouched() {
        let blue = [0.05, 0.05, 0.9];
        let mut hsl = HslParams::default();
        hsl.bands[0] = HslBand {
            hue: 0.8,
            saturation: -0.8,
            luminance: 0.8,
        };
        let out = apply_hsl(blue, &hsl);
        for (a, b) in blue.iter().zip(out.iter()) {
            assert!(
                (a - b).abs() < 1e-3,
                "a red-band edit should not move a pure-blue pixel: {a} vs {b}"
            );
        }
    }

    #[test]
    fn apply_hsl_saturation_band_reduces_delta_for_a_matching_hue() {
        let red = [0.8, 0.2, 0.2];
        let mut hsl = HslParams::default();
        hsl.bands[0].saturation = -0.9; // red band, desaturate hard
        let out = apply_hsl(red, &hsl);
        let out_delta = out[0].max(out[1]).max(out[2]) - out[0].min(out[1]).min(out[2]);
        let in_delta = red[0].max(red[1]).max(red[2]) - red[0].min(red[1]).min(red[2]);
        assert!(
            out_delta < in_delta,
            "desaturating the red band should shrink a red pixel's channel spread: {out_delta} vs {in_delta}"
        );
    }

    #[test]
    fn apply_hsl_hue_shift_rotates_a_red_pixel_toward_the_next_band() {
        let red = [0.8, 0.2, 0.2];
        let mut hsl = HslParams::default();
        hsl.bands[0].hue = 1.0; // red band, rotate hue positively (toward orange)
        let out = apply_hsl(red, &hsl);
        // Rotating red toward orange should increase the green channel relative to blue.
        assert!(
            out[1] > red[1] || out[1] > out[2],
            "hue rotation toward orange should raise green relative to input/blue: out={out:?}"
        );
    }

    #[test]
    fn rgb_hue_degrees_matches_known_primaries() {
        assert!((rgb_hue_degrees([1.0, 0.0, 0.0], 1.0, 1.0) - 0.0).abs() < 1e-3);
        assert!((rgb_hue_degrees([0.0, 1.0, 0.0], 1.0, 1.0) - 120.0).abs() < 1e-3);
        assert!((rgb_hue_degrees([0.0, 0.0, 1.0], 1.0, 1.0) - 240.0).abs() < 1e-3);
    }

    #[test]
    fn hsv_to_rgb_round_trips_known_primaries() {
        let red = hsv_to_rgb(0.0, 1.0, 1.0);
        assert!((red[0] - 1.0).abs() < 1e-5 && red[1].abs() < 1e-5 && red[2].abs() < 1e-5);
        let green = hsv_to_rgb(120.0, 1.0, 1.0);
        assert!(green[0].abs() < 1e-5 && (green[1] - 1.0).abs() < 1e-5 && green[2].abs() < 1e-5);
    }

    #[test]
    fn hsl_band_weight_sums_to_one_between_adjacent_band_centers() {
        // The raised-cosine (Hann) window is a partition of unity: at the midpoint between two
        // adjacent band centers (22.5 degrees), the two overlapping bands' weights must sum to
        // exactly 1.0 -- not just be equal to each other -- so a hue exactly between two band
        // centers gets the same total influence as a hue exactly at one. A squared version of
        // this window (an earlier draft of this function) breaks that property (0.25+0.25=0.5,
        // not 1.0), which is what this test's tighter assertion below is a regression guard for.
        let w0 = hsl_band_weight(22.5, 0);
        let w1 = hsl_band_weight(22.5, 1);
        assert!((w0 - w1).abs() < 1e-4, "{w0} vs {w1}");
        assert!(
            (w0 + w1 - 1.0).abs() < 1e-4,
            "w0+w1={} should be 1.0",
            w0 + w1
        );
        assert!(w0 > 0.0 && w0 < 1.0);
    }

    #[test]
    fn hsl_band_weight_is_zero_beyond_45_degrees() {
        assert_eq!(hsl_band_weight(50.0, 0), 0.0);
        assert_eq!(hsl_band_weight(310.0, 0), 0.0); // -50 deg wrapped
    }

    #[test]
    fn srgb_oetf_matches_known_reference_points() {
        assert!((srgb_oetf(0.0) - 0.0).abs() < 1e-6);
        assert!((srgb_oetf(1.0) - 1.0).abs() < 1e-6);
        // 18% mid-gray linear -> ~0.4614 in sRGB gamma space (1.055*0.18^(1/2.4) - 0.055).
        assert!((srgb_oetf(0.18) - 0.4614).abs() < 0.001);
    }
}
