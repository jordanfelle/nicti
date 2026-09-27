//! Full-frame-vs-subject-crop ablation. The user's issue comment on #35 pointed at the Fursee
//! paper's YOLO-head-crop step, but also noted it may be overkill for photobooth-style shots where
//! the subject is already framed -- this module makes that an ablation to measure, not an
//! assumption either way, rather than committing to YOLO (#108's own scope) up front.
//!
//! The crop source reuses `spikes/siamese/src/segment.rs`'s `BiRefNet` subject-mask output shape
//! (an `Alpha`: single-channel, row-major, `1.0` = selected) rather than adding a YOLO dependency
//! here -- `siamese` has no real weights obtained either (its own doc comment: ~970MB export,
//! "out of this pass's time budget"), so both the full-frame and crop paths in this spike are
//! exercised against synthetic alpha masks; a real BiRefNet run is equally TBD for both #35 and
//! #48, not a new gap this pass introduces.

/// Single-channel alpha mask, row-major, `1.0` = selected -- same shape/convention as
/// `spikes/siamese/src/segment.rs::Alpha`. Duplicated rather than depending on `siamese` (spikes
/// can't depend on other spikes).
#[derive(Debug, Clone)]
pub struct Alpha {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BBox {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

/// A mask above `threshold` everywhere, or a malformed mask (`data.len() != width * height`,
/// which a hand-supplied `<nef_dir>/masks/<stem>.mask.json` file can trivially produce -- caught
/// by an adversarial review, since nothing upstream of this function validates the file before
/// this pass), yields `None` (no crop possible -- treat as full-frame) rather than an index panic.
pub fn bbox_from_alpha(alpha: &Alpha, threshold: f32) -> Option<BBox> {
    if alpha.data.len() != alpha.width * alpha.height {
        return None;
    }
    let (mut x0, mut y0) = (alpha.width, alpha.height);
    let (mut x1, mut y1) = (0usize, 0usize);
    let mut any = false;
    for y in 0..alpha.height {
        for x in 0..alpha.width {
            if alpha.data[y * alpha.width + x] >= threshold {
                any = true;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    if !any {
        return None;
    }
    Some(BBox { x0, y0, x1, y1 })
}

/// Expands `bbox` by `pad_frac` of its own width/height on each side (a crop tight to the mask
/// boundary risks clipping ears/edges the embedder would otherwise see context around), clamped to
/// `(width, height)`.
pub fn pad_bbox(bbox: BBox, pad_frac: f64, width: usize, height: usize) -> BBox {
    let w = bbox.x1 - bbox.x0;
    let h = bbox.y1 - bbox.y0;
    let pad_x = (w as f64 * pad_frac).round() as usize;
    let pad_y = (h as f64 * pad_frac).round() as usize;
    BBox {
        x0: bbox.x0.saturating_sub(pad_x),
        y0: bbox.y0.saturating_sub(pad_y),
        x1: (bbox.x1 + pad_x).min(width),
        y1: (bbox.y1 + pad_y).min(height),
    }
}

/// Crops `img` to `bbox`. Callers doing the full-frame arm of the ablation simply skip calling
/// this and embed `img` directly -- there is no "full-frame crop" variant to keep symmetric with.
pub fn crop_image(img: &image::RgbImage, bbox: BBox) -> image::RgbImage {
    let w = (bbox.x1 - bbox.x0) as u32;
    let h = (bbox.y1 - bbox.y0) as u32;
    image::imageops::crop_imm(img, bbox.x0 as u32, bbox.y0 as u32, w, h).to_image()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_with_square(width: usize, height: usize, sq: BBox) -> Alpha {
        let mut data = vec![0.0f32; width * height];
        for y in sq.y0..sq.y1 {
            for x in sq.x0..sq.x1 {
                data[y * width + x] = 1.0;
            }
        }
        Alpha {
            width,
            height,
            data,
        }
    }

    #[test]
    fn bbox_from_alpha_finds_tight_bounding_box() {
        let alpha = alpha_with_square(
            10,
            10,
            BBox {
                x0: 2,
                y0: 3,
                x1: 6,
                y1: 8,
            },
        );
        let bbox = bbox_from_alpha(&alpha, 0.5).expect("mask has selected pixels");
        assert_eq!(
            bbox,
            BBox {
                x0: 2,
                y0: 3,
                x1: 6,
                y1: 8
            }
        );
    }

    #[test]
    fn bbox_from_alpha_returns_none_for_malformed_data_length_instead_of_panicking() {
        // Regression test for an adversarial-review-caught bug: a hand-supplied
        // `<nef_dir>/masks/<stem>.mask.json` with a `data` array shorter (or longer) than
        // `width * height` used to index-panic instead of returning a clean `None`.
        let alpha = Alpha {
            width: 100,
            height: 100,
            data: vec![1.0f32; 5],
        };
        assert!(bbox_from_alpha(&alpha, 0.5).is_none());
    }

    #[test]
    fn bbox_from_alpha_returns_none_for_empty_mask() {
        let alpha = Alpha {
            width: 4,
            height: 4,
            data: vec![0.0; 16],
        };
        assert!(bbox_from_alpha(&alpha, 0.5).is_none());
    }

    #[test]
    fn pad_bbox_expands_and_clamps_to_image_bounds() {
        let bbox = BBox {
            x0: 2,
            y0: 2,
            x1: 4,
            y1: 4,
        };
        let padded = pad_bbox(bbox, 1.0, 10, 10); // pad = 100% of width/height (2 each side)
        assert_eq!(
            padded,
            BBox {
                x0: 0,
                y0: 0,
                x1: 6,
                y1: 6
            }
        );
    }

    #[test]
    fn pad_bbox_clamps_at_small_image_edge() {
        let bbox = BBox {
            x0: 0,
            y0: 0,
            x1: 8,
            y1: 8,
        };
        let padded = pad_bbox(bbox, 0.5, 10, 10);
        assert_eq!(
            padded,
            BBox {
                x0: 0,
                y0: 0,
                x1: 10,
                y1: 10
            }
        );
    }

    #[test]
    fn crop_image_produces_expected_dimensions() {
        let img = image::RgbImage::from_pixel(20, 20, image::Rgb([1, 2, 3]));
        let cropped = crop_image(
            &img,
            BBox {
                x0: 2,
                y0: 3,
                x1: 10,
                y1: 15,
            },
        );
        assert_eq!(cropped.dimensions(), (8, 12));
    }
}
