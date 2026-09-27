//! Preview-to-full-res edge-aware upsample for AI mask alpha, via a guided filter (He, Sun & Tang,
//! "Guided Image Filtering", 2010/2012 -- the fast variant, filtering at the low resolution and
//! upsampling the linear coefficients rather than the whole image). This is the refinement step
//! #44's own ticket body expects: "Masks at preview res first, refined to full res via edge-aware
//! upsample (guided filter) only when zoomed/exporting."
//!
//! A guided filter (rather than a plain bilinear/bicubic upsample of the alpha itself) keeps the
//! mask's edge snapped to the actual image's edges at full res, instead of a soft blur across a
//! subject boundary that a bilinear alpha upsample alone would produce -- the guidance image is
//! the full-res photo's own luminance.

use crate::image::{Field, Image};

/// Box-filter (mean over a `(2r+1)x(2r+1)` window) a `Field` in place -- the shared primitive
/// every step of the guided filter below uses. Naive O(width*height*r^2), fine at spike/preview
/// scale; a real integration would use a summed-area table.
fn box_filter(field: &Field, radius: usize) -> Field {
    let r = radius as i32;
    let mut out = Field::new(field.width, field.height, 0.0);
    for y in 0..field.height as i32 {
        for x in 0..field.width as i32 {
            let mut sum = 0.0f32;
            let mut count = 0.0f32;
            for dy in -r..=r {
                for dx in -r..=r {
                    if let Some(i) = field.index(x + dx, y + dy) {
                        sum += field.data[i];
                        count += 1.0;
                    }
                }
            }
            out.set(x, y, if count > 0.0 { sum / count } else { 0.0 });
        }
    }
    out
}

fn luminance_field(image: &Image) -> Field {
    let mut out = Field::new(image.width, image.height, 0.0);
    for i in 0..image.data.len() {
        let [r, g, b, _] = image.data[i];
        out.data[i] = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    }
    out
}

fn elementwise(a: &Field, b: &Field, f: impl Fn(f32, f32) -> f32) -> Field {
    let mut out = Field::new(a.width, a.height, 0.0);
    for i in 0..out.data.len() {
        out.data[i] = f(a.data[i], b.data[i]);
    }
    out
}

/// Bilinear-resamples `field` to `(new_width, new_height)`. Samples are **clamped to the source's
/// own edge** (not zero-padded) -- `Field::get` returns `0.0` for an out-of-range index, which
/// would otherwise pull every border-adjacent output pixel toward zero (most of the image, when
/// upsampling a small low-res field by a large factor, since the half-pixel-offset sample
/// coordinate falls outside the source array for roughly the outer half of every source cell).
fn resample_bilinear(field: &Field, new_width: usize, new_height: usize) -> Field {
    let mut out = Field::new(new_width, new_height, 0.0);
    if field.width == 0 || field.height == 0 || new_width == 0 || new_height == 0 {
        return out;
    }
    let scale_x = field.width as f32 / new_width as f32;
    let scale_y = field.height as f32 / new_height as f32;
    let clamp_x = |x: i32| x.clamp(0, field.width as i32 - 1);
    let clamp_y = |y: i32| y.clamp(0, field.height as i32 - 1);
    for oy in 0..new_height {
        for ox in 0..new_width {
            let sx = (ox as f32 + 0.5) * scale_x - 0.5;
            let sy = (oy as f32 + 0.5) * scale_y - 0.5;
            let x0 = sx.floor();
            let y0 = sy.floor();
            let (tx, ty) = (sx - x0, sy - y0);
            let (x0, y0) = (x0 as i32, y0 as i32);
            let (x0c, x1c) = (clamp_x(x0), clamp_x(x0 + 1));
            let (y0c, y1c) = (clamp_y(y0), clamp_y(y0 + 1));
            let p00 = field.get(x0c, y0c);
            let p10 = field.get(x1c, y0c);
            let p01 = field.get(x0c, y1c);
            let p11 = field.get(x1c, y1c);
            let top = p00 + (p10 - p00) * tx;
            let bottom = p01 + (p11 - p01) * tx;
            out.data[oy * new_width + ox] = top + (bottom - top) * ty;
        }
    }
    out
}

/// Refines `low_res_alpha` (an AI mask computed at preview resolution) to `guidance`'s full
/// resolution, using `guidance`'s own luminance as the edge signal. `radius` is the guided
/// filter's box-filter window (in low-res pixels); `eps` is the regularization term (He et al.'s
/// own suggested range is roughly `(0.001..0.1)^2` on `[0,1]`-scaled input -- lower keeps edges
/// sharper but is noisier).
pub fn guided_upsample(low_res_alpha: &Field, guidance: &Image, radius: usize, eps: f32) -> Field {
    let (full_w, full_h) = (guidance.width, guidance.height);
    let (low_w, low_h) = (low_res_alpha.width, low_res_alpha.height);
    if low_w == 0 || low_h == 0 {
        return Field::new(full_w, full_h, 0.0);
    }

    let guidance_lr = luminance_field(guidance).resample_nearest(low_w, low_h);
    let p = low_res_alpha;

    let mean_i = box_filter(&guidance_lr, radius);
    let mean_p = box_filter(p, radius);
    let corr_i = box_filter(
        &elementwise(&guidance_lr, &guidance_lr, |a, b| a * b),
        radius,
    );
    let corr_ip = box_filter(&elementwise(&guidance_lr, p, |a, b| a * b), radius);

    let mut a = Field::new(low_w, low_h, 0.0);
    let mut b = Field::new(low_w, low_h, 0.0);
    for i in 0..a.data.len() {
        let var_i = corr_i.data[i] - mean_i.data[i] * mean_i.data[i];
        let cov_ip = corr_ip.data[i] - mean_i.data[i] * mean_p.data[i];
        let a_i = cov_ip / (var_i + eps);
        a.data[i] = a_i;
        b.data[i] = mean_p.data[i] - a_i * mean_i.data[i];
    }

    let mean_a = box_filter(&a, radius);
    let mean_b = box_filter(&b, radius);

    let mean_a_full = resample_bilinear(&mean_a, full_w, full_h);
    let mean_b_full = resample_bilinear(&mean_b, full_w, full_h);
    let guidance_full = luminance_field(guidance);

    let mut out = Field::new(full_w, full_h, 0.0);
    for i in 0..out.data.len() {
        out.data[i] =
            (mean_a_full.data[i] * guidance_full.data[i] + mean_b_full.data[i]).clamp(0.0, 1.0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_low_res_mask_upsamples_to_a_flat_full_res_mask() {
        let low = Field::new(4, 4, 0.7);
        let guidance = Image::new(32, 32, [0.5, 0.5, 0.5, 1.0]);
        let full = guided_upsample(&low, &guidance, 1, 1e-4);
        for &v in &full.data {
            assert!((v - 0.7).abs() < 0.05, "expected ~0.7, got {v}");
        }
    }

    #[test]
    fn output_resolution_matches_the_guidance_image() {
        let low = Field::new(8, 6, 0.5);
        let guidance = Image::new(64, 48, [0.4, 0.4, 0.4, 1.0]);
        let full = guided_upsample(&low, &guidance, 2, 1e-3);
        assert_eq!(full.width, 64);
        assert_eq!(full.height, 48);
    }

    #[test]
    fn output_stays_within_unit_range() {
        let mut low = Field::new(6, 6, 0.0);
        for (i, v) in low.data.iter_mut().enumerate() {
            *v = if i % 2 == 0 { 1.0 } else { 0.0 };
        }
        let mut guidance = Image::new(48, 48, [0.0, 0.0, 0.0, 1.0]);
        for y in 0..48 {
            for x in 0..48 {
                let v = if (x / 8 + y / 8) % 2 == 0 { 0.9 } else { 0.1 };
                guidance.set(x, y, [v, v, v, 1.0]);
            }
        }
        let full = guided_upsample(&low, &guidance, 2, 1e-3);
        for &v in &full.data {
            assert!((0.0..=1.0).contains(&v), "value {v} out of [0,1]");
        }
    }

    #[test]
    fn a_real_edge_in_guidance_sharpens_a_softly_sampled_low_res_boundary() {
        // Low-res mask has a soft, blurred transition (as a small downsampled AI output would).
        let mut low = Field::new(16, 16, 0.0);
        for y in 0..16 {
            for x in 0..16 {
                low.data[y * 16 + x] = if x < 7 {
                    1.0
                } else if x < 9 {
                    0.5
                } else {
                    0.0
                };
            }
        }
        // High-res guidance has a sharp edge at the same relative location.
        let mut guidance = Image::new(64, 64, [0.0, 0.0, 0.0, 1.0]);
        for y in 0..64 {
            for x in 0..64 {
                let v = if x < 32 { 0.9 } else { 0.1 };
                guidance.set(x, y, [v, v, v, 1.0]);
            }
        }
        let full = guided_upsample(&low, &guidance, 3, 1e-4);
        // Well inside the "should be foreground" region and well inside "should be background",
        // the refined mask should be closer to 1.0/0.0 respectively than a plain bilinear
        // upsample of the soft low-res mask would be at the same relative position.
        assert!(full.get(8, 32) > 0.6);
        assert!(full.get(56, 32) < 0.4);
    }
}
