//! The Develop panel's live histogram (#46). This is a plain CPU implementation, not a GPU
//! kernel -- `nicti-pelt`'s `DevelopView::histogram` builds it from a `frame::read_frame` CPU
//! readback of the current render, display-encoded via `crate::geometry::output_encode`. ADR-0016's
//! "never a full-frame host<->device round-trip in the hot path" concern is about a real full-res
//! photo; this view still renders a small synthetic frame (#31 hasn't landed a real NEF yet), so
//! the readback cost here is trivial. A throttled/GPU histogram is a follow-up once #31 replaces
//! the synthetic frame with a real one -- see `render-graph`'s own "#46 completion" REFERENCE.md
//! bullet.

/// R/G/B/luma counts across 256 buckets each, from a display-encoded (sRGB gamma, `0.0..=1.0`)
/// image -- the same space `crate::geometry::output_encode` produces, matching what a photo
/// editor's histogram conventionally plots (post-tone-curve, post-gamma), not the linear working
/// space every other stage in this crate operates in.
#[derive(Debug, Clone, PartialEq)]
pub struct Histogram {
    pub r: [u32; 256],
    pub g: [u32; 256],
    pub b: [u32; 256],
    pub luma: [u32; 256],
}

fn bucket(display_value: f32) -> usize {
    (display_value.clamp(0.0, 1.0) * 255.0).round() as usize
}

/// Builds a [`Histogram`] from an already display-encoded RGBA buffer (as `read_frame` -> a
/// display-encode pass would produce; the alpha channel is ignored).
pub fn from_display_pixels(pixels: &[[f32; 4]]) -> Histogram {
    let mut hist = Histogram {
        r: [0; 256],
        g: [0; 256],
        b: [0; 256],
        luma: [0; 256],
    };
    for &[r, g, b, _a] in pixels {
        hist.r[bucket(r)] += 1;
        hist.g[bucket(g)] += 1;
        hist.b[bucket(b)] += 1;
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        hist.luma[bucket(luma)] += 1;
    }
    hist
}

impl Histogram {
    pub fn total(&self) -> u32 {
        self.luma.iter().sum()
    }

    /// Nearest-rank percentile (`p` in `0.0..=100.0`) over the luma channel, matching
    /// `spikes/pupil::histogram::Histogram::percentile`'s own semantics so `crate::perk`'s port of
    /// `pupil::heuristic` can read this histogram's bins the same way that spike reads its own
    /// sorted samples.
    pub fn luma_percentile(&self, p: f64) -> f32 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        let rank = ((p / 100.0) * (total as f64 - 1.0)).round().max(0.0) as u32;
        let mut cumulative = 0u32;
        for (i, &count) in self.luma.iter().enumerate() {
            cumulative += count;
            if cumulative > rank {
                return i as f32 / 255.0;
            }
        }
        1.0
    }

    pub fn luma_mean(&self) -> f32 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        let sum: f64 = self
            .luma
            .iter()
            .enumerate()
            .map(|(i, &count)| (i as f64 / 255.0) * count as f64)
            .sum();
        (sum / total as f64) as f32
    }

    pub fn luma_fraction_below(&self, threshold: f32) -> f64 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        let cutoff = bucket(threshold);
        let below: u32 = self.luma[..cutoff].iter().sum();
        below as f64 / total as f64
    }

    /// Fraction of pixels strictly above `threshold`'s own bucket -- the complement of
    /// [`Self::luma_fraction_below`], which is exclusive of the threshold's bucket on the low
    /// side, so together they leave exactly one bucket (the threshold's own) uncounted by either.
    pub fn luma_fraction_above(&self, threshold: f32) -> f64 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        let cutoff = (bucket(threshold) + 1).min(self.luma.len());
        let above: u32 = self.luma[cutoff..].iter().sum();
        above as f64 / total as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(v: f32) -> [f32; 4] {
        [v, v, v, 1.0]
    }

    #[test]
    fn from_display_pixels_counts_every_pixel_once() {
        let pixels = vec![gray(0.0), gray(0.5), gray(1.0)];
        let hist = from_display_pixels(&pixels);
        assert_eq!(hist.total(), 3);
    }

    #[test]
    fn luma_percentile_of_a_uniform_gray_image_is_that_gray_value() {
        let pixels = vec![gray(0.5); 100];
        let hist = from_display_pixels(&pixels);
        assert!((hist.luma_percentile(50.0) - 0.5).abs() < 0.01);
    }

    #[test]
    fn luma_mean_matches_a_known_average() {
        let pixels = vec![gray(0.0), gray(1.0)];
        let hist = from_display_pixels(&pixels);
        assert!((hist.luma_mean() - 0.5).abs() < 0.01);
    }

    #[test]
    fn luma_fraction_below_and_above_are_complementary_around_a_threshold() {
        let mut pixels = vec![gray(0.1); 20];
        pixels.extend(vec![gray(0.9); 80]);
        let hist = from_display_pixels(&pixels);
        let below = hist.luma_fraction_below(0.5);
        let above = hist.luma_fraction_above(0.5);
        assert!((below - 0.2).abs() < 0.02, "below={below}");
        assert!((above - 0.8).abs() < 0.02, "above={above}");
    }

    #[test]
    fn empty_histogram_returns_zero_not_nan() {
        let hist = from_display_pixels(&[]);
        assert_eq!(hist.luma_percentile(50.0), 0.0);
        assert_eq!(hist.luma_mean(), 0.0);
        assert_eq!(hist.luma_fraction_below(0.5), 0.0);
    }
}
