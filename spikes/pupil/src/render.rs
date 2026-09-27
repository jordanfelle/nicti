//! The fixed color treatment #99 applies to get a default-rendered proxy image to analyze --
//! LRC's own "Auto Settings" is computed against the image at its default (all-sliders-zero)
//! render, so the auto-tone histogram must come from that, not from raw camera values. Camera
//! RGB -> XYZ(D50) -> linear sRGB -> sRGB OETF, the same fallback fixed treatment `spikes/rods`
//! already established for exactly this "not ADR-0038's real DCP pipeline, correct enough to
//! compare on equal footing" purpose (see `rods::display`'s module doc comment) -- copied here
//! rather than depended on, per this repo's spikes-stay-self-contained convention. Adds as-shot
//! white balance (`input::white_balance`) before the matrix step, since #40's harness compared
//! already-white-balanced (`dump-classic`) inputs and didn't need this step itself.

use crate::input::{linearize_sample, white_balance, LinearInput};

/// Bradford-adapted XYZ(D50) -> linear sRGB(D65), the standard matrix used throughout ICC/color
/// management tooling -- a published mathematical constant, not project-specific data.
const XYZ_D50_TO_LINEAR_SRGB: [[f64; 3]; 3] = [
    [3.1338561, -1.6168667, -0.4906146],
    [-0.9787684, 1.9161415, 0.0334540],
    [0.0719453, -0.2289914, 1.4052427],
];

/// Camera RGB -> XYZ(D50), from `retina`'s `cam_xyz` sidecar field: row-major 4x3 (up to 4 camera
/// channels x XYZ, unused rows zero). `XYZ_k = sum_c camRGB[c] * cam_xyz[c*3+k]`.
pub fn camera_rgb_to_xyz(cam_rgb: [f64; 3], cam_xyz: &[f32; 12]) -> [f64; 3] {
    std::array::from_fn(|k| (0..3).map(|c| cam_rgb[c] * cam_xyz[c * 3 + k] as f64).sum())
}

pub fn xyz_to_linear_srgb(xyz: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|j| (0..3).map(|k| XYZ_D50_TO_LINEAR_SRGB[j][k] * xyz[k]).sum())
}

/// The standard sRGB opto-electronic transfer function (linear -> gamma-encoded), IEC 61966-2-1.
pub fn srgb_oetf(linear: f64) -> f64 {
    if linear <= 0.0031308 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// Rec. 709 relative luminance of a clamped, display-encoded sRGB triple.
fn luminance(srgb: [f64; 3]) -> f32 {
    (0.2126 * srgb[0] + 0.7152 * srgb[1] + 0.0722 * srgb[2]) as f32
}

/// Renders `input` at default settings and returns one luminance sample (`0.0..=1.0`) per pixel,
/// skipping every `stride`th pixel in each dimension to keep large frames tractable for a
/// histogram, which doesn't need every pixel. `stride = 1` samples every pixel.
pub fn default_render_luminance(input: &LinearInput, stride: u32) -> Vec<f32> {
    let stride = stride.max(1);
    let meta = &input.meta;
    let mut samples = Vec::new();
    for y in (0..input.image.height()).step_by(stride as usize) {
        for x in (0..input.image.width()).step_by(stride as usize) {
            let px = input.image.get_pixel(x, y).0;
            let cam_rgb = [
                linearize_sample(px[0]),
                linearize_sample(px[1]),
                linearize_sample(px[2]),
            ];
            let wb = white_balance(cam_rgb, &meta.cam_mul);
            let xyz = camera_rgb_to_xyz(wb, &meta.cam_xyz);
            let linear_srgb = xyz_to_linear_srgb(xyz);
            let display: [f64; 3] =
                std::array::from_fn(|c| srgb_oetf(linear_srgb[c].clamp(0.0, 1.0)));
            samples.push(luminance(display));
        }
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY_CAM_XYZ: [f32; 12] = [
        1.0, 0.0, 0.0, //
        0.0, 1.0, 0.0, //
        0.0, 0.0, 1.0, //
        0.0, 0.0, 0.0,
    ];

    #[test]
    fn camera_rgb_to_xyz_identity_matrix_passes_through() {
        let xyz = camera_rgb_to_xyz([0.2, 0.5, 0.8], &IDENTITY_CAM_XYZ);
        assert_eq!(xyz, [0.2, 0.5, 0.8]);
    }

    #[test]
    fn srgb_oetf_matches_known_values() {
        assert_eq!(srgb_oetf(0.0), 0.0);
        assert!((srgb_oetf(1.0) - 1.0).abs() < 1e-9);
        assert!((srgb_oetf(0.001) - 0.001 * 12.92).abs() < 1e-9);
    }

    #[test]
    fn srgb_oetf_is_monotonic() {
        let mut prev = srgb_oetf(0.0);
        let mut v = 0.01;
        while v <= 1.0 {
            let cur = srgb_oetf(v);
            assert!(cur > prev, "not monotonic at {v}: {prev} -> {cur}");
            prev = cur;
            v += 0.01;
        }
    }
}
