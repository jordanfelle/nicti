//! Preview-to-full-res mask refine: a guided filter (He, Sun & Tang, "Guided Image Filtering",
//! 2010/2012 fast variant), per #44's own ticket body: "Masks at preview res first, refined to
//! full res via edge-aware upsample (guided filter) only when zoomed/exporting." Ported from
//! `spikes/siamese/src/refine.rs::guided_upsample` (#48/ADR-0024, already proven correct there:
//! `a_real_edge_in_guidance_sharpens_a_softly_sampled_low_res_boundary`) -- spikes don't depend on
//! each other, so this is a copy, not a reuse-by-dependency, adapted onto this module's own
//! [`Field`] type instead of siamese's `image::Field`.
//!
//! `box_filter` (the mean-over-a-window primitive every step below uses) is the one piece this
//! spike also ports to a GPU compute kernel (`gpu::run_box_filter`, checked against this module's
//! CPU reference in `tests/gpu_parity.rs`) -- proving the mechanism is GPU-portable. Chaining every
//! box-filter pass plus the elementwise variance/covariance math into a single always-GPU pipeline
//! is real additional work left to #45's actual render-engine build, not required to validate
//! ADR-0044's design.

#[derive(Debug, Clone)]
pub struct Field {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

impl Field {
    pub fn new(width: usize, height: usize, fill: f32) -> Self {
        Self {
            width,
            height,
            data: vec![fill; width * height],
        }
    }

    fn index(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            None
        } else {
            Some(y as usize * self.width + x as usize)
        }
    }

    pub fn get(&self, x: usize, y: usize) -> f32 {
        self.data[y * self.width + x]
    }

    fn resample_nearest(&self, new_width: usize, new_height: usize) -> Field {
        let mut out = Field::new(new_width, new_height, 0.0);
        if self.width == 0 || self.height == 0 {
            return out;
        }
        for oy in 0..new_height {
            for ox in 0..new_width {
                let sx = ((ox as f32 + 0.5) * self.width as f32 / new_width as f32) as usize;
                let sy = ((oy as f32 + 0.5) * self.height as f32 / new_height as f32) as usize;
                let sx = sx.min(self.width - 1);
                let sy = sy.min(self.height - 1);
                out.data[oy * new_width + ox] = self.get(sx, sy);
            }
        }
        out
    }
}

/// Box-filter (mean over a `(2r+1)x(2r+1)` window). O(width*height*r^2) -- fine at spike/preview
/// scale (siamese's own note); a real integration would use a summed-area table.
pub fn box_filter(field: &Field, radius: usize) -> Field {
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
            out.data[(y as usize) * field.width + (x as usize)] =
                if count > 0.0 { sum / count } else { 0.0 };
        }
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
            let p00 = field.get(x0c as usize, y0c as usize);
            let p10 = field.get(x1c as usize, y0c as usize);
            let p01 = field.get(x0c as usize, y1c as usize);
            let p11 = field.get(x1c as usize, y1c as usize);
            let top = p00 + (p10 - p00) * tx;
            let bottom = p01 + (p11 - p01) * tx;
            out.data[oy * new_width + ox] = top + (bottom - top) * ty;
        }
    }
    out
}

/// Refines `low_res_alpha` (a mask computed at preview resolution) to `(full_w, full_h)`, using
/// `guidance_luma` (the full-resolution photo's own luminance, already extracted by the caller --
/// unlike siamese's version this doesn't take a whole RGBA `Image`, since loaf has no image type
/// of its own beyond `geometry::Plane`, and luminance extraction isn't this module's concern).
pub fn guided_upsample(
    low_res_alpha: &Field,
    guidance_luma: &Field,
    radius: usize,
    eps: f32,
) -> Field {
    let (full_w, full_h) = (guidance_luma.width, guidance_luma.height);
    let (low_w, low_h) = (low_res_alpha.width, low_res_alpha.height);
    if low_w == 0 || low_h == 0 {
        return Field::new(full_w, full_h, 0.0);
    }

    let guidance_lr = guidance_luma.resample_nearest(low_w, low_h);
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

    let mut out = Field::new(full_w, full_h, 0.0);
    for i in 0..out.data.len() {
        out.data[i] =
            (mean_a_full.data[i] * guidance_luma.data[i] + mean_b_full.data[i]).clamp(0.0, 1.0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_low_res_mask_upsamples_to_a_flat_full_res_mask() {
        let low = Field::new(4, 4, 0.7);
        let guidance = Field::new(32, 32, 0.5);
        let full = guided_upsample(&low, &guidance, 1, 1e-4);
        for &v in &full.data {
            assert!((v - 0.7).abs() < 0.05, "expected ~0.7, got {v}");
        }
    }

    #[test]
    fn a_real_edge_in_guidance_sharpens_a_softly_sampled_low_res_boundary() {
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
        let mut guidance = Field::new(64, 64, 0.0);
        for y in 0..64 {
            for x in 0..64 {
                guidance.data[y * 64 + x] = if x < 32 { 0.9 } else { 0.1 };
            }
        }
        let full = guided_upsample(&low, &guidance, 3, 1e-4);
        assert!(full.get(8, 32) > 0.6);
        assert!(full.get(56, 32) < 0.4);
    }

    #[test]
    fn box_filter_of_a_constant_field_is_unchanged() {
        let field = Field::new(10, 10, 0.42);
        let filtered = box_filter(&field, 2);
        for &v in &filtered.data {
            assert!((v - 0.42).abs() < 1e-5);
        }
    }
}
