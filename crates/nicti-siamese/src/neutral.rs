//! The neutral model image (ADR-0048): what every segmentation model is shown.
//!
//! It is built from the *baked* frame (linear camera RGB, post-lens, pre-heal) through
//! `nicti_groom`'s `SpaceMap` -- as-shot white balance, one exposure scale from the image's own
//! highlights, the sRGB curve -- so it never depends on a slider. That is the whole point: a
//! contrast or exposure edit must not be able to change what a model selects, so a tone drag can
//! never re-run one. It is also downscaled here (`ModelFrame`, longest side 1024) so a full 45 MP
//! frame is read once, not once per model.

use std::sync::Arc;

use nicti_groom::sam::ModelFrame;
use nicti_groom::space::SpaceMap;
use nicti_groom::PixelSource;
use nicti_stalk::ModelImage;

/// An owned neutral image: display-referred sRGB `0..=1`, interleaved `HWC`.
#[derive(Debug, Clone, PartialEq)]
pub struct NeutralImage {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<f32>,
    pub image_key: u64,
}

impl NeutralImage {
    /// Builds the neutral image of `source`. `cam_mul` are the camera's as-shot multipliers.
    pub fn build(source: &dyn PixelSource, cam_mul: [f32; 4], image_key: u64) -> Self {
        let space = SpaceMap::for_source(cam_mul, source);
        let frame = ModelFrame::build(source, &space);
        Self {
            width: frame.width,
            height: frame.height,
            rgb: frame.rgb255.iter().map(|v| v / 255.0).collect(),
            image_key,
        }
    }

    /// The borrowed view a [`nicti_stalk::Segmenter`] takes.
    pub fn as_model_image(&self) -> ModelImage<'_> {
        ModelImage {
            width: self.width,
            height: self.height,
            rgb: &self.rgb,
            image_key: self.image_key,
        }
    }

    pub fn byte_size(&self) -> usize {
        self.rgb.len() * 4
    }
}

/// How many neutral images are kept: the photo being edited and the one just before it (so
/// flipping back and forth doesn't rebuild).
pub const CAPACITY: usize = 2;

/// A tiny most-recently-used cache of neutral images, keyed by photo identity.
#[derive(Default)]
pub struct NeutralCache {
    /// Most recent last.
    entries: Vec<Arc<NeutralImage>>,
}

impl NeutralCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached image for `image_key`, or `build`'s result (which is then cached).
    pub fn get_or_build(
        &mut self,
        image_key: u64,
        build: impl FnOnce() -> NeutralImage,
    ) -> Arc<NeutralImage> {
        if let Some(pos) = self.entries.iter().position(|e| e.image_key == image_key) {
            let hit = self.entries.remove(pos);
            self.entries.push(Arc::clone(&hit));
            return hit;
        }
        let built = Arc::new(build());
        self.entries.push(Arc::clone(&built));
        if self.entries.len() > CAPACITY {
            self.entries.remove(0);
        }
        built
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_groom::RgbBuffer;
    use std::cell::Cell;

    fn buffer(w: u32, h: u32) -> RgbBuffer {
        RgbBuffer {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| {
                    let (x, y) = (i % w, i / w);
                    [
                        0.05 + 0.4 * x as f32 / w as f32,
                        0.2 + 0.4 * y as f32 / h as f32,
                        0.1,
                    ]
                })
                .collect(),
        }
    }

    #[test]
    fn a_neutral_image_is_display_referred_and_sized_for_the_model() {
        let img = NeutralImage::build(&buffer(40, 30), [2.0, 1.0, 1.6, 1.0], 9);
        assert_eq!((img.width, img.height), (1024, 768), "longest side is 1024");
        assert_eq!(img.rgb.len(), 1024 * 768 * 3);
        assert!(img.rgb.iter().all(|v| (0.0..=1.0).contains(v)));
        assert_eq!(img.image_key, 9);
        let m = img.as_model_image();
        assert!(m.validate().is_ok());
        assert_eq!((m.width, m.height, m.image_key), (1024, 768, 9));
    }

    #[test]
    fn the_neutral_image_ignores_everything_but_the_photo() {
        // Two builds of the same photo are identical: nothing else can reach it.
        let a = NeutralImage::build(&buffer(16, 16), [2.0, 1.0, 1.5, 1.0], 1);
        let b = NeutralImage::build(&buffer(16, 16), [2.0, 1.0, 1.5, 1.0], 1);
        assert_eq!(a, b);
    }

    #[test]
    fn the_cache_hits_by_photo_and_evicts_the_oldest() {
        let mut cache = NeutralCache::new();
        let builds = Cell::new(0);
        let get = |cache: &mut NeutralCache, key: u64| {
            cache.get_or_build(key, || {
                builds.set(builds.get() + 1);
                NeutralImage {
                    width: 1,
                    height: 1,
                    rgb: vec![0.0; 3],
                    image_key: key,
                }
            })
        };
        get(&mut cache, 1);
        get(&mut cache, 1);
        assert_eq!(builds.get(), 1, "the second request is a hit");
        get(&mut cache, 2);
        get(&mut cache, 3); // evicts 1 (least recently used)
        assert_eq!(cache.len(), CAPACITY);
        get(&mut cache, 2);
        assert_eq!(builds.get(), 3, "2 survived");
        get(&mut cache, 1);
        assert_eq!(builds.get(), 4, "1 was evicted and had to be rebuilt");
    }

    #[test]
    fn touching_an_entry_protects_it_from_eviction() {
        let mut cache = NeutralCache::new();
        let make = |key| NeutralImage {
            width: 1,
            height: 1,
            rgb: vec![0.0; 3],
            image_key: key,
        };
        cache.get_or_build(1, || make(1));
        cache.get_or_build(2, || make(2));
        cache.get_or_build(1, || make(1)); // 1 is now the most recent
        cache.get_or_build(3, || make(3)); // evicts 2, not 1
        let mut rebuilt = false;
        cache.get_or_build(1, || {
            rebuilt = true;
            make(1)
        });
        assert!(!rebuilt);
    }
}
