//! Synthetic degradation of real keeper images (#34/ADR-0034's ground-truth substitute this pass
//! -- see the ADR's Context: no unculled con shoot exists on disk, so real blurry/misfocused
//! rejects mostly don't either. A real photographer's own keepers are overwhelmingly sharp and
//! eyes-open, so this module also enables the *false-flag* measurement: run every candidate over
//! undegraded keepers and see how often it wrongly flags one -- that number is real, not
//! synthetic, and is this pass's headline result (per the ADR's decision rule).
//!
//! Every kernel here is a plain, separable-where-possible convolution over `f32` samples --
//! deliberately not reusing `fast_image_resize`'s resampling kernels, which are built for
//! resizing, not for applying an arbitrary blur PSF at a fixed output size.

use image::{Rgb, RgbImage};

/// A convolution kernel: `(width, height, taps)`, row-major, already normalized to sum to 1.0
/// (callers only need to build the tap pattern, not worry about renormalizing).
pub struct Kernel {
    pub width: u32,
    pub height: u32,
    pub taps: Vec<f32>,
}

impl Kernel {
    fn normalize(mut taps: Vec<f32>) -> Vec<f32> {
        let sum: f32 = taps.iter().sum();
        if sum > 0.0 {
            for t in &mut taps {
                *t /= sum;
            }
        }
        taps
    }

    /// A disk (pillbox) kernel of the given radius -- the textbook defocus PSF for a circular
    /// aperture, uniform weight inside the disk, zero outside.
    pub fn disk(radius: u32) -> Kernel {
        let r = radius as i32;
        let size = (2 * r + 1) as u32;
        let mut taps = vec![0.0f32; (size * size) as usize];
        let r2 = (r * r) as f32;
        for dy in -r..=r {
            for dx in -r..=r {
                if (dx * dx + dy * dy) as f32 <= r2 {
                    let idx = ((dy + r) as u32 * size + (dx + r) as u32) as usize;
                    taps[idx] = 1.0;
                }
            }
        }
        Kernel {
            width: size,
            height: size,
            taps: Self::normalize(taps),
        }
    }

    /// A linear motion-blur kernel: `length` taps along `angle_degrees` (0 = horizontal), each
    /// weighted 1.0 -- a straight-line PSF, the standard synthetic motion-blur model.
    pub fn motion(length: u32, angle_degrees: f32) -> Kernel {
        let length = length.max(1);
        let half = (length as f32 - 1.0) / 2.0;
        let rad = angle_degrees.to_radians();
        let (dx, dy) = (rad.cos(), rad.sin());

        // Bounding box big enough to hold the line at any angle.
        let extent = (length as f32 * dx.abs().max(dy.abs())).ceil() as i32 + 1;
        let size = (2 * extent + 1).max(1) as u32;
        let center = extent;
        let mut taps = vec![0.0f32; (size * size) as usize];

        for i in 0..length {
            let t = i as f32 - half;
            let x = (center as f32 + t * dx).round() as i32;
            let y = (center as f32 + t * dy).round() as i32;
            if x >= 0 && y >= 0 && (x as u32) < size && (y as u32) < size {
                let idx = (y as u32 * size + x as u32) as usize;
                taps[idx] += 1.0;
            }
        }
        Kernel {
            width: size,
            height: size,
            taps: Self::normalize(taps),
        }
    }
}

/// Full-image convolution, edge-clamped (out-of-bounds samples reuse the nearest edge pixel
/// rather than wrapping or zero-padding, which would darken/blur the image border artificially).
pub fn convolve(img: &RgbImage, kernel: &Kernel) -> RgbImage {
    let (w, h) = img.dimensions();
    let (kw, kh) = (kernel.width as i32, kernel.height as i32);
    let (kcx, kcy) = (kw / 2, kh / 2);

    RgbImage::from_fn(w, h, |x, y| {
        let mut sum = [0.0f32; 3];
        for ky in 0..kh {
            for kx in 0..kw {
                let weight = kernel.taps[(ky * kw + kx) as usize];
                if weight == 0.0 {
                    continue;
                }
                let sx = (x as i32 + kx - kcx).clamp(0, w as i32 - 1) as u32;
                let sy = (y as i32 + ky - kcy).clamp(0, h as i32 - 1) as u32;
                let p = img.get_pixel(sx, sy);
                sum[0] += p[0] as f32 * weight;
                sum[1] += p[1] as f32 * weight;
                sum[2] += p[2] as f32 * weight;
            }
        }
        Rgb([
            sum[0].round().clamp(0.0, 255.0) as u8,
            sum[1].round().clamp(0.0, 255.0) as u8,
            sum[2].round().clamp(0.0, 255.0) as u8,
        ])
    })
}

/// Blurs only `(x, y, w, h)` of `img` with `kernel`, leaving the rest untouched -- synthetic
/// misfocus: a subject region that's soft while the rest of the frame (potentially the true
/// background) stays sharp, the inverse of the usual "blur everything" degradation.
pub fn convolve_region(img: &RgbImage, kernel: &Kernel, region: (u32, u32, u32, u32)) -> RgbImage {
    let full = convolve(img, kernel);
    let (rx, ry, rw, rh) = region;
    let mut out = img.clone();
    let (w, h) = img.dimensions();
    let x1 = (rx + rw).min(w);
    let y1 = (ry + rh).min(h);
    for y in ry.min(h)..y1 {
        for x in rx.min(w)..x1 {
            out.put_pixel(x, y, *full.get_pixel(x, y));
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Degradation {
    /// Uniform disk-kernel defocus at the given radius.
    Defocus { radius: u32 },
    /// Linear motion blur at the given length/angle.
    Motion { length: u32, angle_degrees: i32 },
    /// Defocus confined to one region (a synthetic back/front-focus), rest of the frame untouched.
    Misfocus {
        radius: u32,
        region: (u32, u32, u32, u32),
    },
}

impl Degradation {
    pub fn apply(&self, img: &RgbImage) -> RgbImage {
        match *self {
            Degradation::Defocus { radius } => convolve(img, &Kernel::disk(radius)),
            Degradation::Motion {
                length,
                angle_degrees,
            } => convolve(img, &Kernel::motion(length, angle_degrees as f32)),
            Degradation::Misfocus { radius, region } => {
                convolve_region(img, &Kernel::disk(radius), region)
            }
        }
    }

    pub fn label(&self) -> String {
        match *self {
            Degradation::Defocus { radius } => format!("defocus_r{radius}"),
            Degradation::Motion {
                length,
                angle_degrees,
            } => format!("motion_l{length}_a{angle_degrees}"),
            Degradation::Misfocus { radius, .. } => format!("misfocus_r{radius}"),
        }
    }
}

/// The severity sweep #34's synthetic-measurement pass reports against (ADR-0034's Measured
/// results table): a handful of representative radii/lengths/angles, not an exhaustive grid.
pub fn default_sweep() -> Vec<Degradation> {
    let mut out = vec![
        Degradation::Defocus { radius: 1 },
        Degradation::Defocus { radius: 3 },
        Degradation::Defocus { radius: 6 },
        Degradation::Defocus { radius: 10 },
    ];
    for &length in &[3, 8, 16] {
        for &angle in &[0, 45, 90] {
            out.push(Degradation::Motion {
                length,
                angle_degrees: angle,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkerboard(width: u32, height: u32, period: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            if (x / period + y / period).is_multiple_of(2) {
                Rgb([230, 230, 230])
            } else {
                Rgb([20, 20, 20])
            }
        })
    }

    #[test]
    fn disk_kernel_normalizes_to_one() {
        let k = Kernel::disk(3);
        let sum: f32 = k.taps.iter().sum();
        assert!((sum - 1.0).abs() < 1e-4);
    }

    #[test]
    fn motion_kernel_normalizes_to_one() {
        let k = Kernel::motion(9, 30.0);
        let sum: f32 = k.taps.iter().sum();
        assert!((sum - 1.0).abs() < 1e-4);
    }

    #[test]
    fn convolve_flattens_high_contrast_pattern() {
        let img = checkerboard(32, 32, 4);
        let blurred = convolve(&img, &Kernel::disk(4));

        let variance = |im: &RgbImage| -> f64 {
            let vals: Vec<f64> = im.pixels().map(|p| p[0] as f64).collect();
            let mean = vals.iter().sum::<f64>() / vals.len() as f64;
            vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64
        };
        assert!(variance(&blurred) < variance(&img) / 2.0);
    }

    #[test]
    fn convolve_region_leaves_rest_of_frame_untouched() {
        let img = checkerboard(32, 32, 4);
        let out = convolve_region(&img, &Kernel::disk(4), (0, 0, 16, 16));
        // Outside the region: byte-identical to the source.
        for y in 16..32 {
            for x in 16..32 {
                assert_eq!(out.get_pixel(x, y), img.get_pixel(x, y));
            }
        }
        // Inside the region: different from the source (actually blurred).
        let mut any_different = false;
        for y in 0..16 {
            for x in 0..16 {
                if out.get_pixel(x, y) != img.get_pixel(x, y) {
                    any_different = true;
                }
            }
        }
        assert!(any_different, "expected the region to actually be blurred");
    }
}
