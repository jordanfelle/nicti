//! Crop/resize/feather-blend compositing for LaMa-style inpainting -- pure, model-independent
//! math, testable without any real ONNX weights. The pipeline this supports: (1) find the mask's
//! bounding box, (2) expand it by a context margin, (3) crop + resize to the model's fixed input
//! size, (4) run the (stubbed, see `ai.rs`) model, (5) blend the result back into the original
//! image using a feathered version of the mask so the crop boundary doesn't leave a seam.

use crate::cpu_reference::Image;

/// An axis-aligned pixel-space bounding box. `x`/`y` may be negative or extend past the image
/// bounds before `clamp_to_image` is applied -- callers are expected to clamp before cropping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BBox {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl BBox {
    pub fn clamp_to_image(self, image_width: u32, image_height: u32) -> BBox {
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = (self.x + self.width as i32).min(image_width as i32);
        let y1 = (self.y + self.height as i32).min(image_height as i32);
        BBox {
            x: x0,
            y: y0,
            width: (x1 - x0).max(0) as u32,
            height: (y1 - y0).max(0) as u32,
        }
    }
}

/// The tightest bounding box containing every `true` cell of `mask` (row-major, `width * height`
/// long). Returns `None` for an all-`false` mask.
pub fn mask_bounding_box(mask: &[bool], width: usize, height: usize) -> Option<BBox> {
    let (mut min_x, mut min_y) = (usize::MAX, usize::MAX);
    let (mut max_x, mut max_y) = (0usize, 0usize);
    let mut any = false;
    for y in 0..height {
        for x in 0..width {
            if mask[y * width + x] {
                any = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if !any {
        return None;
    }
    Some(BBox {
        x: min_x as i32,
        y: min_y as i32,
        width: (max_x - min_x + 1) as u32,
        height: (max_y - min_y + 1) as u32,
    })
}

/// Expands `bbox` by `margin` pixels on every side (LaMa-style inpainting needs surrounding
/// context, not just the masked pixels themselves), then clamps to the image bounds.
pub fn expand_bbox_with_margin(
    bbox: BBox,
    margin: u32,
    image_width: u32,
    image_height: u32,
) -> BBox {
    let margin = margin as i32;
    let expanded = BBox {
        x: bbox.x - margin,
        y: bbox.y - margin,
        width: bbox.width + 2 * margin as u32,
        height: bbox.height + 2 * margin as u32,
    };
    expanded.clamp_to_image(image_width, image_height)
}

/// Crops `image` to `bbox` (which must already be clamped to the image bounds -- out-of-bounds
/// reads inside `Image::get` would otherwise silently return transparent black rather than erroring).
pub fn crop_image(image: &Image, bbox: BBox) -> Image {
    let mut out = Image::new(bbox.width as usize, bbox.height as usize, [0.0; 4]);
    for y in 0..bbox.height as i32 {
        for x in 0..bbox.width as i32 {
            out.set(x, y, image.get(bbox.x + x, bbox.y + y));
        }
    }
    out
}

/// Simple bilinear resize -- no external `image` crate dependency, since a spike doesn't need
/// format decode, only the resampling math this ticket cares about.
pub fn resize_bilinear(image: &Image, new_width: usize, new_height: usize) -> Image {
    let mut out = Image::new(new_width, new_height, [0.0; 4]);
    if new_width == 0 || new_height == 0 || image.width == 0 || image.height == 0 {
        return out;
    }
    let scale_x = image.width as f32 / new_width as f32;
    let scale_y = image.height as f32 / new_height as f32;
    for oy in 0..new_height {
        for ox in 0..new_width {
            let sx = (ox as f32 + 0.5) * scale_x - 0.5;
            let sy = (oy as f32 + 0.5) * scale_y - 0.5;
            let x0 = sx.floor();
            let y0 = sy.floor();
            let tx = sx - x0;
            let ty = sy - y0;
            let (x0, y0) = (x0 as i32, y0 as i32);

            let p00 = image.get(x0, y0);
            let p10 = image.get(x0 + 1, y0);
            let p01 = image.get(x0, y0 + 1);
            let p11 = image.get(x0 + 1, y0 + 1);

            let mut px = [0.0f32; 4];
            for c in 0..4 {
                let top = p00[c] + (p10[c] - p00[c]) * tx;
                let bottom = p01[c] + (p11[c] - p01[c]) * tx;
                px[c] = top + (bottom - top) * ty;
            }
            out.set(ox as i32, oy as i32, px);
        }
    }
    out
}

/// Feathers a hard boolean mask into a `[0, 1]` alpha field: `1.0` deep inside the mask, ramping
/// to `0.0` over `feather_px` pixels of (approximate) distance from the mask boundary, `0.0`
/// outside. Uses a bounded-radius nearest-boundary-pixel search rather than a true distance
/// transform -- adequate for a spike's mask sizes, not the production algorithm.
pub fn feather_mask(mask: &[bool], width: usize, height: usize, feather_px: f32) -> Vec<f32> {
    if feather_px <= 0.0 {
        return mask.iter().map(|&m| if m { 1.0 } else { 0.0 }).collect();
    }
    let r = feather_px.ceil() as i32;
    let mut out = vec![0.0f32; width * height];
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            if !mask[i] {
                out[i] = 0.0;
                continue;
            }
            // Distance to the nearest `false` cell within the search radius; if none is found,
            // this pixel is deep inside the mask and gets full weight.
            let mut min_dist = feather_px;
            let mut found_edge = false;
            'search: for dy in -r..=r {
                for dx in -r..=r {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    let outside_mask = nx < 0
                        || ny < 0
                        || nx as usize >= width
                        || ny as usize >= height
                        || !mask[ny as usize * width + nx as usize];
                    if outside_mask {
                        let dist = ((dx * dx + dy * dy) as f32).sqrt();
                        if dist < min_dist {
                            min_dist = dist;
                            found_edge = true;
                        }
                        if min_dist <= 0.0 {
                            break 'search;
                        }
                    }
                }
            }
            out[i] = if found_edge {
                (min_dist / feather_px).clamp(0.0, 1.0)
            } else {
                1.0
            };
        }
    }
    out
}

/// Composites `inpainted_crop` (already resized back to `crop_bbox`'s own dimensions) into
/// `original` at `crop_bbox`'s location, weighted per-pixel by `feathered_mask` (same dimensions
/// as `inpainted_crop`, values in `[0, 1]`): `out = original * (1 - w) + inpainted * w`.
pub fn composite_inpainted_crop(
    original: &Image,
    inpainted_crop: &Image,
    crop_bbox: BBox,
    feathered_mask: &[f32],
) -> Image {
    assert_eq!(
        feathered_mask.len(),
        (inpainted_crop.width * inpainted_crop.height),
        "feathered_mask must match the inpainted crop's own dimensions"
    );
    let mut out = original.clone();
    for cy in 0..inpainted_crop.height as i32 {
        for cx in 0..inpainted_crop.width as i32 {
            let w = feathered_mask[cy as usize * inpainted_crop.width + cx as usize];
            if w <= 0.0 {
                continue;
            }
            let (px, py) = (crop_bbox.x + cx, crop_bbox.y + cy);
            let base = out.get(px, py);
            let painted = inpainted_crop.get(cx, cy);
            let mut blended = [0.0f32; 4];
            for c in 0..4 {
                blended[c] = base[c] * (1.0 - w) + painted[c] * w;
            }
            out.set(px, py, blended);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask_rect(
        width: usize,
        height: usize,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
    ) -> Vec<bool> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (x0..x1).contains(&x) && (y0..y1).contains(&y)))
            .collect()
    }

    #[test]
    fn bounding_box_matches_the_rect() {
        let mask = mask_rect(32, 32, 10, 12, 20, 18);
        let bbox = mask_bounding_box(&mask, 32, 32).unwrap();
        assert_eq!(
            bbox,
            BBox {
                x: 10,
                y: 12,
                width: 10,
                height: 6
            }
        );
    }

    #[test]
    fn empty_mask_has_no_bounding_box() {
        let mask = vec![false; 16];
        assert!(mask_bounding_box(&mask, 4, 4).is_none());
    }

    #[test]
    fn margin_expands_and_clamps() {
        let bbox = BBox {
            x: 2,
            y: 2,
            width: 4,
            height: 4,
        };
        let expanded = expand_bbox_with_margin(bbox, 3, 100, 100);
        // x=2-3=-1 clamps to 0, so the clamped width loses the 1px that fell off the left edge:
        // 9, not the full unclamped 10.
        assert_eq!(
            expanded,
            BBox {
                x: 0,
                y: 0,
                width: 9,
                height: 9
            }
        );
        let mid = BBox {
            x: 50,
            y: 50,
            width: 4,
            height: 4,
        };
        let expanded_mid = expand_bbox_with_margin(mid, 3, 100, 100);
        assert_eq!(
            expanded_mid,
            BBox {
                x: 47,
                y: 47,
                width: 10,
                height: 10
            }
        );
    }

    #[test]
    fn crop_extracts_expected_pixels() {
        let mut img = Image::new(10, 10, [0.0; 4]);
        img.set(5, 5, [1.0, 0.0, 0.0, 1.0]);
        let bbox = BBox {
            x: 3,
            y: 3,
            width: 4,
            height: 4,
        };
        let cropped = crop_image(&img, bbox);
        assert_eq!(cropped.get(2, 2), [1.0, 0.0, 0.0, 1.0]); // (5,5) - (3,3) = (2,2)
    }

    #[test]
    fn resize_preserves_a_flat_field() {
        let img = Image::new(8, 8, [0.4, 0.5, 0.6, 1.0]);
        let resized = resize_bilinear(&img, 16, 16);
        for c in 0..3 {
            assert!((resized.get(8, 8)[c] - img.get(4, 4)[c]).abs() < 1e-5);
        }
    }

    #[test]
    fn feather_mask_is_full_weight_deep_inside_and_zero_outside() {
        let mask = mask_rect(20, 20, 5, 5, 15, 15);
        let feathered = feather_mask(&mask, 20, 20, 3.0);
        assert_eq!(feathered[10 * 20 + 10], 1.0); // deep interior
        assert_eq!(feathered[0], 0.0); // outside the mask entirely
        let edge = feathered[5 * 20 + 5]; // exactly on the mask boundary
        assert!(edge > 0.0 && edge < 1.0);
    }

    #[test]
    fn composite_blend_math_is_a_weighted_average() {
        let original = Image::new(4, 4, [0.0, 0.0, 0.0, 1.0]);
        let inpainted = Image::new(4, 4, [1.0, 1.0, 1.0, 1.0]);
        let feathered = vec![0.25f32; 16];
        let bbox = BBox {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        let out = composite_inpainted_crop(&original, &inpainted, bbox, &feathered);
        for c in 0..3 {
            assert!((out.get(0, 0)[c] - 0.25).abs() < 1e-6);
        }
    }

    #[test]
    fn composite_leaves_untouched_pixels_unchanged() {
        let mut original = Image::new(6, 6, [0.0, 0.0, 0.0, 1.0]);
        original.set(0, 0, [0.9, 0.9, 0.9, 1.0]);
        let inpainted = Image::new(2, 2, [1.0, 1.0, 1.0, 1.0]);
        let feathered = vec![1.0f32; 4];
        let bbox = BBox {
            x: 4,
            y: 4,
            width: 2,
            height: 2,
        };
        let out = composite_inpainted_crop(&original, &inpainted, bbox, &feathered);
        assert_eq!(out.get(0, 0), [0.9, 0.9, 0.9, 1.0]);
    }
}
