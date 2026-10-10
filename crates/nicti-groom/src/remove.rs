//! The removal pipeline: prompt -> object mask -> crop with context -> LaMa -> `RemovalPatch`.
//!
//! [`RemovalEngine`] is generic over the [`Segmenter`] and [`Inpainter`] so all the geometry and
//! color handling is tested with fakes; the real ONNX-backed pair is `MobileSam` + `Lama`.

use nicti_tapetum::heal::{RemovalPatch, MAX_PATCH_SIDE};

use crate::geom::{self, Rect};
use crate::lama::{self, Inpainter};
use crate::sam::{ModelFrame, Prompt, SegMask, Segmenter};
use crate::space::SpaceMap;
use crate::{PixelSource, RemovalError};

/// Extra context around the object, as a fraction of the object's size on each side.
const CONTEXT_FRAC: f32 = 0.75;
/// Smallest crop (source px): LaMa needs some surroundings even for a tiny blemish.
const MIN_CROP: i32 = 160;
/// Largest crop (source px), odd, and within `RemovalPatch`'s own limit.
const MAX_CROP: i32 = 2047;
/// Widest feather (source px) around the filled hole; see where it is used for why it is capped.
const MAX_FEATHER_PX: f32 = 6.0;
/// Camera-linear fill values are clamped to this. Normalized sensor values live around [0, ~1]; the
/// cap only matters for extreme white-balance gains, where an unbounded value would overflow to
/// infinity in the f16 texture and turn into NaN in the blend.
const MAX_FILL: f32 = 1000.0;
/// Largest object (source px across) the pipeline will attempt: past this a 512x512 inpaint is
/// stretched too thin to look right, so it is refused rather than returning a smear.
pub const MAX_OBJECT: i32 = 1024;

const _: () = assert!(MAX_CROP as u32 <= MAX_PATCH_SIDE && MAX_CROP % 2 == 1);

/// One removal request, everything in source-image pixels.
pub struct RemovalRequest<'a> {
    /// Identifies the photo (cache key for the color mapping, model frame and SAM embedding).
    pub image_key: u64,
    pub source: &'a dyn PixelSource,
    /// The camera's as-shot white-balance multipliers (`LinearFrame::cam_mul`).
    pub cam_mul: [f32; 4],
    pub prompt: Prompt,
    /// The spot's circle: the removal never reaches farther than `radius` from `center`, so the
    /// spot's size control is also the "how much" control.
    pub center: (f32, f32),
    pub radius: f32,
}

/// Anything that can turn a [`RemovalRequest`] into a patch (the real engine, or a test double).
pub trait RemovalBackend {
    fn remove(&mut self, req: &RemovalRequest<'_>) -> Result<RemovalPatch, RemovalError>;
}

struct Cached {
    key: u64,
    space: SpaceMap,
    frame: ModelFrame,
}

pub struct RemovalEngine<S: Segmenter, I: Inpainter> {
    segmenter: S,
    inpainter: I,
    cached: Option<Cached>,
}

impl<S: Segmenter, I: Inpainter> RemovalEngine<S, I> {
    pub fn new(segmenter: S, inpainter: I) -> Self {
        Self {
            segmenter,
            inpainter,
            cached: None,
        }
    }
}

/// Bilinear sample of the mask logits at fractional model-frame coordinates (pixel centers at
/// `+0.5`), edge-clamped.
fn sample_logit(mask: &SegMask, fx: f32, fy: f32) -> f32 {
    let x = (fx - 0.5).clamp(0.0, (mask.width - 1) as f32);
    let y = (fy - 0.5).clamp(0.0, (mask.height - 1) as f32);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(mask.width - 1), (y0 + 1).min(mask.height - 1));
    let (tx, ty) = (x - x0 as f32, y - y0 as f32);
    let at = |xx: usize, yy: usize| mask.logits[yy * mask.width + xx];
    let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
    let bot = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
    top * (1.0 - ty) + bot * ty
}

impl<S: Segmenter, I: Inpainter> RemovalBackend for RemovalEngine<S, I> {
    fn remove(&mut self, req: &RemovalRequest<'_>) -> Result<RemovalPatch, RemovalError> {
        let (w, h) = (req.source.width() as i32, req.source.height() as i32);
        if w == 0 || h == 0 {
            return Err(RemovalError::BadInput("the image is empty".into()));
        }
        if !(req.radius.is_finite()
            && req.radius > 0.0
            && req.center.0.is_finite()
            && req.center.1.is_finite())
        {
            return Err(RemovalError::BadInput(
                "the spot needs a finite center and a positive radius".into(),
            ));
        }

        // Color mapping + model-frame image, once per photo.
        if self.cached.as_ref().is_none_or(|c| c.key != req.image_key) {
            let space = SpaceMap::for_source(req.cam_mul, req.source);
            let frame = ModelFrame::build(req.source, &space);
            self.cached = Some(Cached {
                key: req.image_key,
                space,
                frame,
            });
        }
        let cached = self.cached.as_ref().expect("just ensured");
        let (space, frame) = (cached.space, &cached.frame);

        // 1. Object mask in the model frame, limited to the spot's circle.
        let seg = self.segmenter.segment(
            req.image_key,
            frame,
            req.prompt.scaled(frame.scale_x, frame.scale_y),
        )?;
        if seg.logits.len() != seg.width * seg.height || seg.width == 0 || seg.height == 0 {
            return Err(RemovalError::BadInput(
                "the segmenter returned a malformed mask".into(),
            ));
        }

        // Object bounds in source pixels: scan the model-frame mask restricted to the circle.
        let (cx, cy) = req.center;
        let in_disc =
            |sx: f32, sy: f32| (sx - cx).powi(2) + (sy - cy).powi(2) <= req.radius * req.radius;
        let model_fg: Vec<bool> = (0..seg.width * seg.height)
            .map(|i| {
                let (mx, my) = ((i % seg.width) as f32 + 0.5, (i / seg.width) as f32 + 0.5);
                seg.logits[i] > 0.0 && in_disc(mx / frame.scale_x, my / frame.scale_y)
            })
            .collect();
        let bounds =
            geom::mask_bounds(&model_fg, seg.width, seg.height).ok_or(RemovalError::NoObject)?;
        let obj = Rect {
            x0: ((bounds.x0 as f32 / frame.scale_x).floor() as i32).clamp(0, w),
            y0: ((bounds.y0 as f32 / frame.scale_y).floor() as i32).clamp(0, h),
            x1: ((bounds.x1 as f32 / frame.scale_x).ceil() as i32).clamp(0, w),
            y1: ((bounds.y1 as f32 / frame.scale_y).ceil() as i32).clamp(0, h),
        };
        let obj_side = obj.width().max(obj.height()).max(1);
        if obj_side > MAX_OBJECT {
            return Err(RemovalError::RegionTooLarge(obj_side, MAX_OBJECT));
        }

        // 2. A square crop with context, inside the image, odd-sided.
        let want = (obj_side as f32 * (1.0 + 2.0 * CONTEXT_FRAC)).ceil() as i32;
        let mut side = want.clamp(MIN_CROP, MAX_CROP).min(w.min(h));
        if side % 2 == 0 {
            side -= 1;
        }
        let grow = ((side as f32 * 0.01).max(3.0)).round();
        if (obj_side as f32) + 2.0 * grow > side as f32 {
            // The object (plus its grown edge) does not fit in the largest square the image allows.
            return Err(RemovalError::RegionTooLarge(obj_side, side));
        }
        let (ocx, ocy) = obj.center();
        let x0 = (ocx - side / 2).clamp(0, w - side);
        let y0 = (ocy - side / 2).clamp(0, h - side);
        let s = side as usize;

        // 3. Crop pixels (camera space) and the hole mask at crop resolution.
        let mut cam: Vec<[f32; 3]> = Vec::with_capacity(s * s);
        let mut hole: Vec<bool> = Vec::with_capacity(s * s);
        for j in 0..s {
            for i in 0..s {
                let (sx, sy) = ((x0 + i as i32) as u32, (y0 + j as i32) as u32);
                cam.push(req.source.pixel(sx, sy));
                let (fx, fy) = (sx as f32 + 0.5, sy as f32 + 0.5);
                hole.push(
                    in_disc(fx, fy)
                        && sample_logit(&seg, fx * frame.scale_x, fy * frame.scale_y) > 0.0,
                );
            }
        }
        if !hole.iter().any(|&b| b) {
            return Err(RemovalError::NoObject);
        }
        let hole = geom::dilate(&hole, s, s, grow);

        // 4. Inpaint at 512x512.
        let model_px: Vec<[f32; 3]> = cam.iter().map(|&p| space.to_model(p)).collect();
        let small = geom::resize_bilinear(&model_px, s, s, lama::SIZE, lama::SIZE);
        let mut image_chw = vec![0.0f32; 3 * lama::SIZE * lama::SIZE];
        for (i, px) in small.iter().enumerate() {
            for c in 0..3 {
                image_chw[c * lama::SIZE * lama::SIZE + i] = px[c];
            }
        }
        let hole_f: Vec<[f32; 1]> = hole.iter().map(|&b| [f32::from(b)]).collect();
        let mask_small: Vec<f32> = geom::resize_bilinear(&hole_f, s, s, lama::SIZE, lama::SIZE)
            .into_iter()
            .map(|m| if m[0] > 0.5 { 1.0 } else { 0.0 })
            .collect();
        let out_chw = self.inpainter.inpaint(&image_chw, &mask_small)?;
        if out_chw.len() != image_chw.len() {
            return Err(RemovalError::BadInput(
                "the inpainter returned a wrongly sized image".into(),
            ));
        }

        // 5. Back to crop resolution and camera space, weighted by the feathered hole.
        let plane = lama::SIZE * lama::SIZE;
        let out_px: Vec<[f32; 3]> = (0..plane)
            .map(|i| [out_chw[i], out_chw[plane + i], out_chw[2 * plane + i]])
            .collect();
        let filled = geom::resize_bilinear(&out_px, lama::SIZE, lama::SIZE, s, s);
        // Kept narrow: outside the hole the fill is the model's copy of the *original* round-tripped
        // through a 512 px resize and the display mapping, i.e. slightly blurred and highlight-
        // clipped, and the feather blends that over the real pixels. A wide ring would visibly
        // soften real detail around every removal.
        let feather = ((side as f32 * 0.015).clamp(3.0, MAX_FEATHER_PX)).round();
        let weight = geom::feathered_weight(&hole, s, s, feather);
        let pixels: Vec<[f32; 4]> = filled
            .iter()
            .zip(&weight)
            .map(|(&m, &wgt)| {
                if wgt > 0.0 {
                    let c = space.to_camera(m).map(|v| v.clamp(0.0, MAX_FILL));
                    [c[0], c[1], c[2], wgt]
                } else {
                    [0.0; 4]
                }
            })
            .collect();
        RemovalPatch::new((x0 + side / 2, y0 + side / 2), side as u32, pixels)
            .map_err(|e| RemovalError::BadInput(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RgbBuffer;

    /// A "segmenter" that marks a disc of `r` source px around the click as foreground.
    struct DiscSegmenter {
        r: f32,
        calls: u32,
        embeds: u32,
        last_key: Option<u64>,
    }

    impl DiscSegmenter {
        fn new(r: f32) -> Self {
            Self {
                r,
                calls: 0,
                embeds: 0,
                last_key: None,
            }
        }
    }

    impl Segmenter for DiscSegmenter {
        fn segment(
            &mut self,
            key: u64,
            frame: &ModelFrame,
            prompt: Prompt,
        ) -> Result<SegMask, RemovalError> {
            self.calls += 1;
            if self.last_key != Some(key) {
                self.embeds += 1;
                self.last_key = Some(key);
            }
            let Prompt::Click { x, y } = prompt else {
                return Err(RemovalError::BadInput(
                    "test segmenter only does clicks".into(),
                ));
            };
            let r = self.r * frame.scale_x;
            let logits = (0..frame.width * frame.height)
                .map(|i| {
                    let (mx, my) = (
                        (i % frame.width) as f32 + 0.5,
                        (i / frame.width) as f32 + 0.5,
                    );
                    r - ((mx - x).powi(2) + (my - y).powi(2)).sqrt()
                })
                .collect();
            Ok(SegMask {
                width: frame.width,
                height: frame.height,
                logits,
            })
        }
    }

    /// Fills the hole with a fixed model-space color and leaves everything else as given.
    struct FillInpainter {
        color: [f32; 3],
        last_mask_sum: f32,
    }

    impl Inpainter for FillInpainter {
        fn inpaint(&mut self, image: &[f32], mask: &[f32]) -> Result<Vec<f32>, RemovalError> {
            let plane = lama::SIZE * lama::SIZE;
            self.last_mask_sum = mask.iter().sum();
            let mut out = image.to_vec();
            for i in 0..plane {
                if mask[i] > 0.5 {
                    for c in 0..3 {
                        out[c * plane + i] = self.color[c];
                    }
                }
            }
            Ok(out)
        }
    }

    fn photo(w: u32, h: u32) -> RgbBuffer {
        RgbBuffer {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| {
                    let (x, y) = ((i % w) as f32 / w as f32, (i / w) as f32 / h as f32);
                    [0.1 + 0.4 * x, 0.15 + 0.4 * y, 0.08 + 0.2 * (x + y) / 2.0]
                })
                .collect(),
        }
    }

    fn engine(r: f32) -> RemovalEngine<DiscSegmenter, FillInpainter> {
        RemovalEngine::new(
            DiscSegmenter::new(r),
            FillInpainter {
                color: [0.5, 0.5, 0.5],
                last_mask_sum: 0.0,
            },
        )
    }

    fn request<'a>(
        img: &'a RgbBuffer,
        at: (f32, f32),
        radius: f32,
        key: u64,
    ) -> RemovalRequest<'a> {
        RemovalRequest {
            image_key: key,
            source: img,
            cam_mul: [2.0, 1.0, 1.5, 1.0],
            prompt: Prompt::Click { x: at.0, y: at.1 },
            center: at,
            radius,
        }
    }

    fn weight_at(patch: &RemovalPatch, x: i32, y: i32) -> f32 {
        let half = patch.side as i32 / 2;
        let (px, py) = (x - patch.center.0 + half, y - patch.center.1 + half);
        if px < 0 || py < 0 || px >= patch.side as i32 || py >= patch.side as i32 {
            return 0.0;
        }
        patch.pixel((py * patch.side as i32 + px) as usize)[3]
    }

    #[test]
    fn a_click_produces_a_centered_odd_square_patch_that_covers_the_object() {
        let img = photo(800, 600);
        let mut e = engine(30.0);
        let patch = e.remove(&request(&img, (400.0, 300.0), 60.0, 1)).unwrap();
        assert_eq!(patch.side % 2, 1);
        // MIN_CROP (160) is even, so the crop rounds down to the next odd side.
        assert!(patch.side as i32 >= MIN_CROP - 1);
        // Patch center is on the object (the crop is centered on it, and it is far from any edge).
        assert!((patch.center.0 - 400).abs() <= 2 && (patch.center.1 - 300).abs() <= 2);
        // Full weight at the object's center, none far outside the hole.
        assert!(weight_at(&patch, 400, 300) > 0.99);
        assert!(weight_at(&patch, 400 + 30 + 40, 300) < 0.01);
        // And every pixel with weight has a finite fill.
        assert!(patch.pixels().all(|p| p.iter().all(|v| v.is_finite())));
    }

    #[test]
    fn the_fill_is_mapped_back_into_camera_space() {
        let img = photo(800, 600);
        let mut e = engine(30.0);
        let patch = e.remove(&request(&img, (400.0, 300.0), 60.0, 1)).unwrap();
        // The test inpainter fills with model-space 0.5 gray; the patch must hold the camera-space
        // value that maps back to exactly that.
        let space = e.cached.as_ref().unwrap().space;
        let expected = space.to_camera([0.5, 0.5, 0.5]);
        let half = patch.side as i32 / 2;
        let px = patch.pixel((half * patch.side as i32 + half) as usize);
        for c in 0..3 {
            assert!(
                (px[c] - expected[c]).abs() < 1e-3,
                "channel {c}: {} vs {}",
                px[c],
                expected[c]
            );
        }
        // White balance makes the camera-space fill non-neutral, which is the point of the mapping.
        assert!((expected[0] - expected[1]).abs() > 1e-3);
    }

    #[test]
    fn the_spots_radius_bounds_how_far_the_removal_reaches() {
        let img = photo(800, 600);
        // The segmenter thinks the object is huge (100 px), but the spot only allows 20.
        let mut e = engine(100.0);
        let patch = e.remove(&request(&img, (400.0, 300.0), 20.0, 1)).unwrap();
        assert!(weight_at(&patch, 400, 300) > 0.99);
        assert!(
            weight_at(&patch, 400 + 20 + 30, 300) < 0.01,
            "must not reach past the circle"
        );
    }

    #[test]
    fn a_click_on_nothing_is_no_object() {
        let img = photo(800, 600);
        let mut e = engine(0.1); // sub-pixel "object": nothing is foreground
        let r = e.remove(&request(&img, (400.0, 300.0), 60.0, 1));
        assert!(matches!(r, Err(RemovalError::NoObject)), "{r:?}");
    }

    #[test]
    fn an_object_bigger_than_the_limit_is_refused() {
        let img = photo(3000, 2000);
        let mut e = engine(900.0); // 1800 px across
        let r = e.remove(&request(&img, (1500.0, 1000.0), 900.0, 1));
        assert!(
            matches!(r, Err(RemovalError::RegionTooLarge(_, MAX_OBJECT))),
            "{r:?}"
        );
    }

    #[test]
    fn an_object_too_big_for_the_smaller_image_side_is_refused() {
        let img = photo(1200, 200); // crop can be at most 199 px
                                    // 194 px across: under MAX_OBJECT, but with its grown edge (3 px each side) it no longer
                                    // fits in the 199 px crop the 200 px image allows.
        let mut e = engine(97.0);
        let r = e.remove(&request(&img, (600.0, 100.0), 100.0, 1));
        assert!(matches!(r, Err(RemovalError::RegionTooLarge(..))), "{r:?}");
    }

    #[test]
    fn near_an_edge_the_crop_slides_inside_the_image() {
        let img = photo(800, 600);
        let mut e = engine(20.0);
        let patch = e.remove(&request(&img, (10.0, 12.0), 30.0, 1)).unwrap();
        let half = patch.side as i32 / 2;
        assert!(patch.center.0 - half >= 0 && patch.center.1 - half >= 0);
        assert!(patch.center.0 + half < 800 && patch.center.1 + half < 600);
        assert!(weight_at(&patch, 10, 12) > 0.99);
    }

    #[test]
    fn a_small_image_gets_a_crop_no_bigger_than_itself() {
        let img = photo(100, 90);
        let mut e = engine(8.0);
        let patch = e.remove(&request(&img, (50.0, 45.0), 12.0, 1)).unwrap();
        assert!(patch.side <= 89 && patch.side % 2 == 1);
        assert!(weight_at(&patch, 50, 45) > 0.99);
    }

    #[test]
    fn the_feather_stays_narrow_even_for_a_huge_crop() {
        // 900 px object -> ~2000 px crop; a proportional feather would be ~30 px.
        let img = photo(4000, 3000);
        let mut e = engine(450.0);
        let patch = e
            .remove(&request(&img, (2000.0, 1500.0), 470.0, 1))
            .unwrap();
        let ring: Vec<f32> = (450..500)
            .map(|d| weight_at(&patch, 2000 + d, 1500))
            .collect();
        // Fully transparent again within (dilation + MAX_FEATHER_PX) of the 450 px mask edge.
        let zero_at = ring
            .iter()
            .position(|&w| w == 0.0)
            .expect("weight reaches 0")
            + 450;
        assert!(
            zero_at <= 450 + 25 + MAX_FEATHER_PX as usize,
            "weight reaches 0 only at {zero_at}"
        );
    }

    #[test]
    fn extreme_white_balance_cannot_produce_non_finite_or_huge_fill() {
        let img = photo(800, 600);
        let mut e = engine(30.0);
        let mut req = request(&img, (400.0, 300.0), 60.0, 1);
        req.cam_mul = [1.0e6, 1.0, 1.0e-6, 1.0];
        let patch = e.remove(&req).unwrap();
        assert!(patch
            .pixels()
            .all(|p| p.iter().all(|v| v.is_finite() && *v <= MAX_FILL)));
    }

    #[test]
    fn the_hole_is_grown_past_the_mask_and_feathered_softly() {
        let img = photo(800, 600);
        let mut e = engine(30.0);
        let patch = e.remove(&request(&img, (400.0, 300.0), 60.0, 1)).unwrap();
        // Just outside the raw 30 px mask: still inside the grown hole.
        assert!(weight_at(&patch, 400 + 31, 300) > 0.99);
        // Somewhere in the feather band the weight is strictly between 0 and 1.
        let band: Vec<f32> = (30..70).map(|d| weight_at(&patch, 400 + d, 300)).collect();
        assert!(band.iter().any(|&w| w > 0.05 && w < 0.95), "{band:?}");
        assert!(
            band.windows(2).all(|p| p[0] >= p[1] - 1e-6),
            "weight must fall monotonically"
        );
    }

    #[test]
    fn the_same_photo_shares_its_model_frame_and_a_new_photo_gets_a_new_one() {
        let img = photo(400, 300);
        let mut e = engine(20.0);
        e.remove(&request(&img, (100.0, 100.0), 40.0, 7)).unwrap();
        e.remove(&request(&img, (250.0, 150.0), 40.0, 7)).unwrap();
        assert_eq!(e.segmenter.calls, 2);
        assert_eq!(e.segmenter.embeds, 1, "same key -> one embedding");
        e.remove(&request(&img, (250.0, 150.0), 40.0, 8)).unwrap();
        assert_eq!(e.segmenter.embeds, 2, "new key -> new embedding");
    }

    #[test]
    fn bad_requests_are_rejected_not_panics() {
        let img = photo(400, 300);
        let mut e = engine(20.0);
        for (c, r) in [
            ((f32::NAN, 5.0), 10.0),
            ((5.0, 5.0), 0.0),
            ((5.0, 5.0), -1.0),
            ((5.0, 5.0), f32::NAN),
        ] {
            let res = e.remove(&request(&img, c, r, 1));
            assert!(
                matches!(res, Err(RemovalError::BadInput(_))),
                "{c:?} {r}: {res:?}"
            );
        }
        let empty = RgbBuffer {
            width: 0,
            height: 0,
            data: vec![],
        };
        assert!(matches!(
            e.remove(&request(&empty, (1.0, 1.0), 5.0, 2)),
            Err(RemovalError::BadInput(_))
        ));
    }

    #[test]
    fn a_segmenter_failure_propagates() {
        struct Failing;
        impl Segmenter for Failing {
            fn segment(
                &mut self,
                _: u64,
                _: &ModelFrame,
                _: Prompt,
            ) -> Result<SegMask, RemovalError> {
                Err(RemovalError::Ort("boom".into()))
            }
        }
        let img = photo(400, 300);
        let mut e = RemovalEngine::new(
            Failing,
            FillInpainter {
                color: [0.0; 3],
                last_mask_sum: 0.0,
            },
        );
        assert!(matches!(
            e.remove(&request(&img, (100.0, 100.0), 30.0, 1)),
            Err(RemovalError::Ort(_))
        ));
    }
}
