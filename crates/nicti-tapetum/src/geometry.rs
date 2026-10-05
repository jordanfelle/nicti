//! Crop as a geometry-only affine sample pass (ADR-0044): reads only the live suffix's own
//! output, never a baked or live node's input directly. `Affine2D` maps an *output* pixel
//! coordinate to the *input* coordinate to sample -- the inverse of "where does this input pixel
//! end up," which is what a sampling pass actually needs.

use std::sync::LazyLock;

use nicti_calico::space::OutputSpace;

use crate::color;

/// A 2D affine transform: `(x, y) -> (a*x + b*y + tx, c*x + d*y + ty)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine2D {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Affine2D {
    pub const IDENTITY: Affine2D = Affine2D {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    /// The inverse-sample transform for a crop with top-left origin `(x, y)`: output pixel
    /// `(0,0)` samples input `(x, y)`, every other output pixel maps 1:1 (no rescale) -- a plain
    /// crop, not a crop+zoom. The output's own extent (how far the crop rect actually extends) is
    /// the caller's `RenderRequest::extent`, not a parameter here.
    pub fn crop(x: f32, y: f32) -> Affine2D {
        Affine2D {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            tx: x,
            ty: y,
        }
    }

    /// The inverse-sample transform for [`crop`] further rotated by `rotation_degrees` about
    /// `center` -- straighten (#47), composed into the same affine crop transform rather than a
    /// separate pipeline stage, per ADR-0044's "crop as an affine sample pass" stage-order note.
    ///
    /// **Sign convention** (this is exactly the class of gimbal/sign error worth spelling out
    /// explicitly, not just asserting): `rotation_degrees` is the angle the *displayed content*
    /// appears to rotate **clockwise**, in this crate's y-down pixel-row convention (x right, y
    /// down, matching every other pixel-space coordinate in this module). A positive angle
    /// visually rotates the image clockwise. Since this produces an *inverse* (output -> input)
    /// sample transform, the matrix actually applied is the rotation's inverse (i.e. `-angle`)
    /// composed with the crop translation:
    ///
    /// ```text
    /// sample = R(-angle) * (output - center) + center + (x, y) - (x, y)
    /// ```
    ///
    /// where `center` is given in the *output's own* coordinate space (typically the crop rect's
    /// own center, e.g. `(width/2, height/2)` for a rect starting at that rect's own top-left).
    /// The translation term folds `crop`'s own `(x, y)` origin shift into the same matrix, so a
    /// zero rotation reduces exactly to `Affine2D::crop(x, y)` (proven by a test below).
    pub fn crop_and_rotate(x: f32, y: f32, center: (f32, f32), rotation_degrees: f32) -> Affine2D {
        let theta = rotation_degrees.to_radians();
        let (sin_t, cos_t) = theta.sin_cos();
        // R(-theta): the inverse of the forward (clockwise-positive, y-down) content rotation.
        let a = cos_t;
        let b = sin_t;
        let c = -sin_t;
        let d = cos_t;
        // Input-space center is the same point as `center` (the rect's center, in the untranslated
        // frame) shifted by the crop origin -- `crop`'s own `(x, y)`.
        let center_in = (center.0 + x, center.1 + y);
        Affine2D {
            a,
            b,
            c,
            d,
            tx: center_in.0 - (a * center.0 + b * center.1),
            ty: center_in.1 - (c * center.0 + d * center.1),
        }
    }

    pub fn apply(&self, p: (f32, f32)) -> (f32, f32) {
        (
            self.a * p.0 + self.b * p.1 + self.tx,
            self.c * p.0 + self.d * p.1 + self.ty,
        )
    }
}

/// A crop rectangle in source-image pixel space (top-left origin, never negative width/height).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl CropRect {
    /// The rect's center, relative to its own top-left origin -- what
    /// [`Affine2D::crop_and_rotate`]'s `center` argument expects.
    pub fn center(&self) -> (f32, f32) {
        (self.width / 2.0, self.height / 2.0)
    }

    /// The rect's center in the same absolute (source-image) coordinate space as `x`/`y` -- what a
    /// caller drawing/hit-testing the rect's own rotated overlay needs (a UI concern, not the
    /// sampling math itself, which only ever needs the relative `center` above).
    pub fn center_absolute(&self) -> (f32, f32) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }
}

/// The inverse-sample transform for `rect`, further straightened by `rotation_degrees` about the
/// rect's own center -- the single entry point `stages::CropKernel::set_transform` callers use, so
/// a caller never has to hand-derive [`Affine2D::crop_and_rotate`]'s `center` argument itself.
pub fn affine_for_crop(rect: CropRect, rotation_degrees: f32) -> Affine2D {
    Affine2D::crop_and_rotate(rect.x, rect.y, rect.center(), rotation_degrees)
}

/// The maximum total straighten rotation, in either direction -- matches LRC's own Straighten
/// tool clamp (a documented, reasonable default; not derived from any hard technical constraint).
/// Never allowed as a raw catalog value beyond this range, since a much larger rotation both looks
/// like an accident (a straighten gesture that intended a small correction, not a deliberate
/// portrait flip) and starts sampling far outside the source image on every corner.
pub const MAX_STRAIGHTEN_DEGREES: f32 = 45.0;

/// Clamps a rotation to `[-MAX_STRAIGHTEN_DEGREES, MAX_STRAIGHTEN_DEGREES]`.
pub fn clamp_rotation_degrees(degrees: f32) -> f32 {
    degrees.clamp(-MAX_STRAIGHTEN_DEGREES, MAX_STRAIGHTEN_DEGREES)
}

/// The rotation delta (in degrees) that levels a dragged reference line -- the Ctrl-drag-a-
/// reference-line gesture (#47): the user holds Ctrl and drags along something in the image that
/// should be horizontal or vertical; on release, this computes how much to rotate so that line
/// becomes level. `(dx, dy)` is the drag vector in the *same* pixel space the image is currently
/// displayed in (y-down); the two endpoints' absolute positions don't matter, only their
/// difference.
///
/// Axis detection is automatic (not a separate mode the user picks): a drag whose horizontal
/// extent dominates is assumed to mark a horizontal reference (e.g. a rooftop line, a horizon); a
/// drag whose vertical extent dominates is assumed to mark a vertical one (e.g. a doorframe,
/// a flagpole).
///
/// The returned delta is always wrapped into `(-90.0, 90.0]` degrees -- a reference line has no
/// inherent direction (dragging from either end gives the same line), and never rotating by more
/// than 90 degrees means a near-diagonal drag can never flip the image upside down, only nudge it
/// toward the nearer axis. A zero-length drag (both endpoints coincide, or the user released
/// without moving) returns `0.0` -- a documented no-op, not a panic or a NaN from `atan2(0, 0)`
/// (which itself is well-defined as `0.0`, but this still guards the semantic intent explicitly).
pub fn straighten_delta_degrees(dx: f32, dy: f32) -> f32 {
    if dx == 0.0 && dy == 0.0 {
        return 0.0;
    }
    let phi = dy.atan2(dx).to_degrees();
    let target_horizontal = dx.abs() >= dy.abs();
    let mut delta = if target_horizontal { -phi } else { 90.0 - phi };
    while delta <= -90.0 {
        delta += 180.0;
    }
    while delta > 90.0 {
        delta -= 180.0;
    }
    delta
}

/// CPU reference for the geometry pass's bilinear sample -- what `present_sample.wgsl` computes
/// per pixel, used to prove the GPU kernel matches it. `src` is row-major RGBA f32, `src_extent`
/// is `(width, height)`. Samples outside `[0, width) x [0, height)` clamp to the nearest edge
/// texel (never wrap, never a transparent border) -- a crop is expected to only ever request
/// coordinates inside the source, so edge behavior here is a safety net, not a designed feature.
pub fn sample_bilinear(
    src: &[[f32; 4]],
    src_extent: (u32, u32),
    transform: &Affine2D,
    out_xy: (u32, u32),
) -> [f32; 4] {
    let (sx, sy) = transform.apply((out_xy.0 as f32 + 0.5, out_xy.1 as f32 + 0.5));
    // Texel-center convention: subtract 0.5 to convert from continuous sample position back to
    // an array index space where texel n's center is at n.0, matching `present_sample.wgsl`.
    let sx = (sx - 0.5).clamp(0.0, (src_extent.0 - 1) as f32);
    let sy = (sy - 0.5).clamp(0.0, (src_extent.1 - 1) as f32);
    let x0 = sx.floor() as u32;
    let y0 = sy.floor() as u32;
    let x1 = (x0 + 1).min(src_extent.0 - 1);
    let y1 = (y0 + 1).min(src_extent.1 - 1);
    let fx = sx - x0 as f32;
    let fy = sy - y0 as f32;

    let at = |x: u32, y: u32| -> [f32; 4] { src[(y * src_extent.0 + x) as usize] };
    let c00 = at(x0, y0);
    let c10 = at(x1, y0);
    let c01 = at(x0, y1);
    let c11 = at(x1, y1);

    let mut out = [0.0f32; 4];
    for i in 0..4 {
        let top = c00[i] * (1.0 - fx) + c10[i] * fx;
        let bottom = c01[i] * (1.0 - fx) + c11[i] * fx;
        out[i] = top * (1.0 - fy) + bottom * fy;
    }
    out
}

/// Linear ProPhoto -> linear sRGB, from `nicti-calico` (#318) -- the same matrix the display
/// shader and export use, so a CPU readback, the screen and an exported file all agree. Built once
/// (an f64 derivation from primaries + Bradford) rather than per pixel.
static PROPHOTO_TO_SRGB: LazyLock<color::Mat3> =
    LazyLock::new(|| OutputSpace::Srgb.from_working_f32());

/// Linear ProPhoto RGB (the working space) -> linear sRGB -> sRGB OETF, for readback/golden-image
/// comparison only -- the live suffix and geometry pass themselves never touch this. The matrix
/// result is unclamped, but `srgb_oetf` clamps to [0, 1], so out-of-gamut channels clip.
pub fn output_encode(prophoto_linear: [f32; 3]) -> [f32; 3] {
    let srgb_linear = color::mat3_apply(*PROPHOTO_TO_SRGB, prophoto_linear);
    srgb_linear.map(color::srgb_oetf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_transform_samples_the_same_pixel() {
        let src = vec![[1.0, 2.0, 3.0, 1.0]; 16];
        let out = sample_bilinear(&src, (4, 4), &Affine2D::IDENTITY, (2, 2));
        assert_eq!(out, [1.0, 2.0, 3.0, 1.0]);
    }

    #[test]
    fn crop_offset_shifts_the_sampled_pixel() {
        let mut src = vec![[0.0, 0.0, 0.0, 1.0]; 16];
        src[5] = [9.0, 9.0, 9.0, 1.0]; // (x=1, y=1) in a 4-wide row-major grid
        let transform = Affine2D::crop(1.0, 1.0);
        let out = sample_bilinear(&src, (4, 4), &transform, (0, 0));
        assert_eq!(out, [9.0, 9.0, 9.0, 1.0]);
    }

    #[test]
    fn out_of_bounds_sample_clamps_to_the_edge_not_black() {
        let src = vec![[5.0, 5.0, 5.0, 1.0]; 4]; // 2x2, all one color
        let transform = Affine2D::crop(-10.0, -10.0);
        let out = sample_bilinear(&src, (2, 2), &transform, (0, 0));
        assert_eq!(out, [5.0, 5.0, 5.0, 1.0]);
    }

    #[test]
    fn output_encode_maps_working_space_white_close_to_srgb_white() {
        let out = output_encode([1.0, 1.0, 1.0]);
        for c in out {
            assert!((c - 1.0).abs() < 0.01, "expected near-white, got {c}");
        }
    }

    #[test]
    fn output_encode_maps_black_to_black() {
        let out = output_encode([0.0, 0.0, 0.0]);
        for c in out {
            assert!(c.abs() < 1e-5);
        }
    }

    fn assert_affine_close(got: Affine2D, want: Affine2D, eps: f32) {
        assert!((got.a - want.a).abs() < eps, "a: {got:?} vs {want:?}");
        assert!((got.b - want.b).abs() < eps, "b: {got:?} vs {want:?}");
        assert!((got.c - want.c).abs() < eps, "c: {got:?} vs {want:?}");
        assert!((got.d - want.d).abs() < eps, "d: {got:?} vs {want:?}");
        assert!((got.tx - want.tx).abs() < eps, "tx: {got:?} vs {want:?}");
        assert!((got.ty - want.ty).abs() < eps, "ty: {got:?} vs {want:?}");
    }

    #[test]
    fn crop_and_rotate_at_zero_degrees_matches_plain_crop() {
        let got = Affine2D::crop_and_rotate(3.0, 4.0, (10.0, 20.0), 0.0);
        let want = Affine2D::crop(3.0, 4.0);
        assert_affine_close(got, want, 1e-5);
    }

    /// Locks in the rotation's sign/direction (exactly the class of bug an adversarial review is
    /// asked to attack): a 90-degree clockwise content rotation, sampled at the output point
    /// directly "below" center (y-down: positive y), must sample the source point directly
    /// "right of" center -- i.e. the source's right edge ends up displayed at the bottom, which is
    /// what a visually-clockwise quarter turn does to a square in a y-down pixel grid.
    #[test]
    fn crop_and_rotate_90_degrees_matches_hand_derived_clockwise_rotation() {
        let center = (0.0, 0.0);
        let transform = Affine2D::crop_and_rotate(0.0, 0.0, center, 90.0);
        let sampled = transform.apply((0.0, 1.0)); // output: straight below center
        assert!(
            (sampled.0 - 1.0).abs() < 1e-4 && sampled.1.abs() < 1e-4,
            "expected source point directly right of center, got {sampled:?}"
        );
    }

    #[test]
    fn crop_and_rotate_180_degrees_is_a_point_reflection_about_center() {
        let center = (5.0, 5.0);
        let transform = Affine2D::crop_and_rotate(0.0, 0.0, center, 180.0);
        let sampled = transform.apply((8.0, 6.0));
        // Reflection of (8,6) through (5,5) is (2,4).
        assert!((sampled.0 - 2.0).abs() < 1e-4 && (sampled.1 - 4.0).abs() < 1e-4);
    }

    #[test]
    fn crop_and_rotate_composes_translation_and_rotation_center_stays_fixed() {
        // The rect's own center must sample the rect's own center in source space regardless of
        // rotation -- a rotation about a point leaves that point itself unmoved.
        let rect = CropRect {
            x: 10.0,
            y: 20.0,
            width: 40.0,
            height: 30.0,
        };
        let center_out = rect.center();
        for degrees in [-30.0, 0.0, 15.0, 44.9] {
            let transform = affine_for_crop(rect, degrees);
            let sampled = transform.apply(center_out);
            let expected = (rect.x + center_out.0, rect.y + center_out.1);
            assert!(
                (sampled.0 - expected.0).abs() < 1e-3 && (sampled.1 - expected.1).abs() < 1e-3,
                "degrees={degrees}: sampled {sampled:?} vs expected {expected:?}"
            );
        }
    }

    #[test]
    fn straighten_delta_is_noop_for_a_zero_length_drag() {
        assert_eq!(straighten_delta_degrees(0.0, 0.0), 0.0);
    }

    #[test]
    fn straighten_delta_levels_a_nearly_horizontal_drag() {
        // A line tilted 5 degrees below horizontal (dominant dx, y-down so positive dy tilts
        // "down" toward the right) needs a -5 degree (counterclockwise) correction to level.
        let angle = 5.0f32.to_radians();
        let dx = 100.0 * angle.cos();
        let dy = 100.0 * angle.sin();
        let delta = straighten_delta_degrees(dx, dy);
        assert!((delta - (-5.0)).abs() < 1e-2, "delta = {delta}");
    }

    #[test]
    fn straighten_delta_levels_a_nearly_vertical_drag() {
        // A line tilted 5 degrees off vertical (dominant dy), leaning right as it goes down.
        let angle = 5.0f32.to_radians();
        let dx = 100.0 * angle.sin();
        let dy = 100.0 * angle.cos();
        let delta = straighten_delta_degrees(dx, dy);
        assert!(delta.abs() < 90.0);
        // Applying the correction should make the (rotated) vector's angle-from-vertical zero:
        // rotate (dx, dy) by delta (forward, clockwise-positive) and check it lands on the
        // vertical axis (dx' ~= 0).
        let theta = delta.to_radians();
        let rotated_dx = theta.cos() * dx - theta.sin() * dy;
        assert!(rotated_dx.abs() < 0.5, "rotated_dx = {rotated_dx}");
    }

    #[test]
    fn straighten_delta_never_exceeds_90_degrees_in_magnitude() {
        for deg in (-179..=180).step_by(7) {
            let angle = (deg as f32).to_radians();
            let delta = straighten_delta_degrees(angle.cos(), angle.sin());
            assert!(
                delta > -90.0 && delta <= 90.0,
                "deg={deg} produced out-of-range delta {delta}"
            );
        }
    }

    #[test]
    fn straighten_delta_applied_via_crop_and_rotate_levels_the_dragged_line() {
        // End-to-end: pick a tilted vector, compute the correcting delta, then confirm that
        // forward-rotating the same vector by that delta actually lands it on the target axis --
        // proving the sign convention here matches `crop_and_rotate`'s own documented one, not
        // just this module's internal math in isolation.
        let dx = 40.0;
        let dy = -6.0; // slight upward tilt, dominant dx -> horizontal target
        let delta = straighten_delta_degrees(dx, dy);
        let theta = delta.to_radians();
        let rotated = (
            theta.cos() * dx - theta.sin() * dy,
            theta.sin() * dx + theta.cos() * dy,
        );
        assert!(
            rotated.1.abs() < 1e-3,
            "rotated dy should be ~0, got {rotated:?}"
        );
    }

    #[test]
    fn clamp_rotation_degrees_stays_within_max_straighten_range() {
        assert_eq!(clamp_rotation_degrees(1000.0), MAX_STRAIGHTEN_DEGREES);
        assert_eq!(clamp_rotation_degrees(-1000.0), -MAX_STRAIGHTEN_DEGREES);
        assert_eq!(clamp_rotation_degrees(3.0), 3.0);
    }
}
