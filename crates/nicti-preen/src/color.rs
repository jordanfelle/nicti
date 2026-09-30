//! Working space -> output space, and quantization (#57).
//!
//! The render's output is linear ProPhoto (D50), unclamped, alpha ignored. Export multiplies by
//! the destination space's matrix (`OutputSpace::from_working_f32`) *without* clipping -- so the
//! watermark can composite in linear light -- and only then applies the output curve and clips, via
//! a lookup table (`OutputSpace::encode` clamps to [0, 1]; NaN maps to 0).
//!
//! No rendering intent or black-point compensation: out-of-gamut colors clip per channel.
//! Gamut-mapping/BPC for export is #320's scope.

use nicti_calico::space::OutputSpace;

use crate::spec::BitDepth;

/// Entries in the encode lookup table. 16384 keeps the step in the darkest (steepest, x12.92)
/// part of the sRGB curve to ~0.2 8-bit levels.
const LUT_SIZE: usize = 16_384;

/// Quantized interleaved RGB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputPixels {
    Rgb8(Vec<u8>),
    Rgb16(Vec<u16>),
}

impl OutputPixels {
    pub fn len(&self) -> usize {
        match self {
            OutputPixels::Rgb8(v) => v.len(),
            OutputPixels::Rgb16(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Multiplies interleaved linear ProPhoto RGB by `space`'s matrix, in place. No clamping.
pub fn to_output_linear(pixels: &mut [f32], space: OutputSpace) {
    apply_matrix(pixels, space.from_working_f32());
}

/// A 3x3 applied to every RGB triple, in place.
pub fn apply_matrix(pixels: &mut [f32], m: [[f32; 3]; 3]) {
    for i in (0..pixels.len() / 3 * 3).step_by(3) {
        let (r, g, b) = (pixels[i], pixels[i + 1], pixels[i + 2]);
        pixels[i] = m[0][0] * r + m[0][1] * g + m[0][2] * b;
        pixels[i + 1] = m[1][0] * r + m[1][1] * g + m[1][2] * b;
        pixels[i + 2] = m[2][0] * r + m[2][1] * g + m[2][2] * b;
    }
}

/// The 3x3 taking linear **sRGB** (a watermark logo's color) into `space`'s linear RGB.
pub fn srgb_to_output_matrix(space: OutputSpace) -> [[f32; 3]; 3] {
    let a = space.from_working_f32();
    let b = OutputSpace::Srgb.to_working_f32();
    let mut m = [[0.0f32; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

/// Applies `space`'s output curve, clips to [0, 1] and quantizes to `depth`.
pub fn quantize(pixels: &[f32], space: OutputSpace, depth: BitDepth) -> OutputPixels {
    let lut: Vec<f32> = (0..=LUT_SIZE)
        .map(|i| space.encode(i as f32 / LUT_SIZE as f32))
        .collect();
    // Linearly interpolated: nearest-neighbour would collapse a 16-bit ramp to ~16k distinct codes
    // (and ~50-code shadow steps). The curve is smooth (and exactly linear near black), so
    // interpolation error at this table size is far below one 16-bit code.
    let encoded = |v: f32| -> f32 {
        if v.is_nan() || v <= 0.0 {
            0.0
        } else if v >= 1.0 {
            lut[LUT_SIZE]
        } else {
            let x = v * LUT_SIZE as f32;
            let i = x as usize;
            let t = x - i as f32;
            lut[i] + (lut[i + 1] - lut[i]) * t
        }
    };
    match depth {
        BitDepth::Eight => OutputPixels::Rgb8(
            pixels
                .iter()
                .map(|&v| (encoded(v) * 255.0).round().clamp(0.0, 255.0) as u8)
                .collect(),
        ),
        BitDepth::Sixteen => OutputPixels::Rgb16(
            pixels
                .iter()
                .map(|&v| (encoded(v) * 65_535.0).round().clamp(0.0, 65_535.0) as u16)
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_white_and_black_survive_the_matrix() {
        for space in OutputSpace::ALL {
            let mut px = [1.0f32, 1.0, 1.0, 0.0, 0.0, 0.0];
            to_output_linear(&mut px, space);
            for v in &px[..3] {
                assert!((v - 1.0).abs() < 1e-3, "{space:?} white {px:?}");
            }
            assert!(px[3..].iter().all(|v| v.abs() < 1e-6));
        }
    }

    #[test]
    fn quantize_matches_the_calico_curve_at_sample_points() {
        for space in OutputSpace::ALL {
            let samples = [0.0f32, 0.001, 0.05, 0.18, 0.5, 0.9, 1.0];
            let OutputPixels::Rgb8(q) = quantize(&samples, space, BitDepth::Eight) else {
                panic!()
            };
            for (s, q) in samples.iter().zip(q) {
                let want = (space.encode(*s) * 255.0).round() as i32;
                assert!((q as i32 - want).abs() <= 1, "{space:?} {s}: {q} vs {want}");
            }
        }
    }

    #[test]
    fn quantize_clips_out_of_range_and_maps_nan_to_zero() {
        let px = [
            -0.5f32,
            f32::NAN,
            2.0,
            f32::INFINITY,
            f32::NEG_INFINITY,
            1.0,
        ];
        let OutputPixels::Rgb8(q) = quantize(&px, OutputSpace::Srgb, BitDepth::Eight) else {
            panic!()
        };
        assert_eq!(q, [0, 0, 255, 255, 0, 255]);
        let OutputPixels::Rgb16(q) = quantize(&px, OutputSpace::Srgb, BitDepth::Sixteen) else {
            panic!()
        };
        assert_eq!(q, [0, 0, 65_535, 65_535, 0, 65_535]);
    }

    #[test]
    fn a_dark_16_bit_ramp_uses_nearly_every_code_not_a_coarse_lut() {
        // 0..0.01 linear spans ~6500 sRGB 16-bit codes; nearest-neighbour lookup gave ~165.
        let px: Vec<f32> = (0..30_000).map(|i| i as f32 / 30_000.0 * 0.01).collect();
        let OutputPixels::Rgb16(q) = quantize(&px, OutputSpace::Srgb, BitDepth::Sixteen) else {
            panic!()
        };
        let distinct: std::collections::BTreeSet<_> = q.iter().collect();
        assert!(
            distinct.len() > 5_500,
            "only {} distinct codes",
            distinct.len()
        );
        // And it still tracks the exact curve.
        for (v, code) in px.iter().zip(&q).step_by(997) {
            let want = OutputSpace::Srgb.encode(*v) * 65_535.0;
            assert!((*code as f32 - want).abs() <= 1.5, "{v}: {code} vs {want}");
        }
    }

    #[test]
    fn sixteen_bit_has_more_resolution_than_eight() {
        let px = [0.2001f32, 0.2, 0.2002];
        let OutputPixels::Rgb16(q) = quantize(&px, OutputSpace::Srgb, BitDepth::Sixteen) else {
            panic!()
        };
        assert!(q[0] != q[1] || q[2] != q[1]);
    }

    #[test]
    fn a_saturated_srgb_red_stays_within_gamut_in_wider_spaces() {
        // sRGB red is inside Adobe RGB and P3, so the converted linear values are in [0, 1].
        for space in [OutputSpace::DisplayP3, OutputSpace::AdobeRgb] {
            let mut px = [1.0f32, 0.0, 0.0];
            apply_matrix(&mut px, srgb_to_output_matrix(space));
            assert!(
                px.iter().all(|v| (-1e-3..=1.0 + 1e-3).contains(v)),
                "{space:?} {px:?}"
            );
        }
        let mut px = [1.0f32, 0.0, 0.0];
        apply_matrix(&mut px, srgb_to_output_matrix(OutputSpace::Srgb));
        assert!((px[0] - 1.0).abs() < 1e-3 && px[1].abs() < 1e-3 && px[2].abs() < 1e-3);
    }
}
