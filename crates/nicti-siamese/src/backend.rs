//! Running a recipe: resolve the provider it names, load it once, run it on the neutral image.
//!
//! [`RegistryBackend`] is the real thing; [`MaskBackend`] is the seam the job talks to so tests
//! can swap in a fake. Loading is **lazy and retried**: nothing is loaded until the first bake
//! (reading and verifying ~1 GB must not happen at start-up for a user who never makes an AI
//! mask), and a failed load is *not* cached -- the user may have finished the download since.
//! Loaded segmenters are also dropped after [`IDLE_UNLOAD_AFTER`] without a bake (#356): BiRefNet
//! is ~970 MB of weights plus activations, which should not stay resident for a whole session
//! because the user selected a subject once. The next bake reloads it lazily (and re-verifies).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nicti_groom::PixelSource;
use nicti_stalk::models::ModelStore;
use nicti_stalk::{
    resolve_provider, AlphaMap, LoadContext, SegmentError, SegmentTarget, SegmentationRegistry,
    Segmenter,
};
use nicti_tapetum::coat::MaskRecipe;

use crate::neutral::{NeutralCache, NeutralImage};

/// How long a loaded segmenter may sit unused before [`MaskBackend::unload_if_idle`] drops it.
pub const IDLE_UNLOAD_AFTER: Duration = Duration::from_secs(5 * 60);

/// One bake: which photo, and which recipe to run on it.
pub struct BakeRequest<'a> {
    /// Identity of the photo (keys the neutral-image cache).
    pub image_key: u64,
    pub source: &'a dyn PixelSource,
    /// The camera's as-shot multipliers (R/G/B/G2), for the neutral image's white balance.
    pub cam_mul: [f32; 4],
    pub recipe: &'a MaskRecipe,
}

/// Something that can run a bake. `Send` so a Pounce worker can own it.
pub trait MaskBackend: Send {
    fn bake(&mut self, req: &BakeRequest<'_>) -> Result<AlphaMap, SegmentError>;

    /// How long until the loaded models have been idle for `ttl` (`Duration::ZERO` = due now);
    /// `None` when nothing is loaded. Lets the caller sleep until then instead of polling.
    fn idle_unload_in(&self, _now: Instant, _ttl: Duration) -> Option<Duration> {
        None
    }

    /// Drops every loaded model if they have been idle for `ttl`; true if it did. Dropping an ONNX
    /// session touches the shared ORT environment, so callers run this on the GPU lane (see
    /// `job::UnloadIdleJob`), serialized with bakes and removals.
    fn unload_if_idle(&mut self, _now: Instant, _ttl: Duration) -> bool {
        false
    }
}

pub struct RegistryBackend {
    registry: Arc<SegmentationRegistry>,
    store: ModelStore,
    ort_dylib: Option<PathBuf>,
    /// Loaded segmenters by `(model_id, model_version)`.
    loaded: HashMap<(String, String), Box<dyn Segmenter>>,
    neutral: NeutralCache,
    /// When the last bake finished (success or not); `None` until the first.
    last_used: Option<Instant>,
}

impl RegistryBackend {
    pub fn new(
        registry: Arc<SegmentationRegistry>,
        store: ModelStore,
        ort_dylib: Option<PathBuf>,
    ) -> Self {
        Self {
            registry,
            store,
            ort_dylib,
            loaded: HashMap::new(),
            neutral: NeutralCache::new(),
            last_used: None,
        }
    }

    /// True once this `(model_id, version)` has been loaded.
    pub fn is_loaded(&self, model_id: &str, model_version: &str) -> bool {
        self.loaded
            .contains_key(&(model_id.to_owned(), model_version.to_owned()))
    }
}

impl MaskBackend for RegistryBackend {
    fn bake(&mut self, req: &BakeRequest<'_>) -> Result<AlphaMap, SegmentError> {
        let result = self.run_bake(req);
        self.last_used = Some(Instant::now());
        result
    }

    fn idle_unload_in(&self, now: Instant, ttl: Duration) -> Option<Duration> {
        if self.loaded.is_empty() {
            return None;
        }
        let idle = now.saturating_duration_since(self.last_used?);
        Some(ttl.saturating_sub(idle))
    }

    fn unload_if_idle(&mut self, now: Instant, ttl: Duration) -> bool {
        if self.idle_unload_in(now, ttl) != Some(Duration::ZERO) {
            return false;
        }
        self.loaded.clear();
        self.neutral = NeutralCache::new();
        true
    }
}

impl RegistryBackend {
    fn run_bake(&mut self, req: &BakeRequest<'_>) -> Result<AlphaMap, SegmentError> {
        let recipe = req.recipe;
        let target = SegmentTarget::from_params(&recipe.params)?;
        let provider = resolve_provider(
            &self.registry,
            &recipe.model_id,
            &recipe.model_version,
            target,
        )?;
        let key = (recipe.model_id.clone(), recipe.model_version.clone());
        if !self.loaded.contains_key(&key) {
            let segmenter = provider.load(&LoadContext {
                store: &self.store,
                ort_dylib: self.ort_dylib.as_deref(),
            })?;
            self.loaded.insert(key.clone(), segmenter);
        }
        let neutral = self.neutral.get_or_build(req.image_key, || {
            NeutralImage::build(req.source, req.cam_mul, req.image_key)
        });
        self.loaded
            .get_mut(&key)
            .expect("just loaded")
            .segment(&neutral.as_model_image(), &recipe.params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::{Descriptor, Module, Registry};
    use nicti_groom::RgbBuffer;
    use nicti_stalk::{ModelImage, ModelProvider, SegmentationProvider};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static LOADS: AtomicUsize = AtomicUsize::new(0);
    static SEGMENTS: AtomicUsize = AtomicUsize::new(0);
    static FAIL_LOADS: AtomicUsize = AtomicUsize::new(0);

    struct Fake;
    impl Module for Fake {
        fn id(&self) -> &str {
            "test.fake"
        }
        fn schema_version(&self) -> u32 {
            1
        }
        fn migrate_params(&self, _v: u32, p: Value) -> Option<Value> {
            Some(p)
        }
    }
    impl ModelProvider for Fake {}
    impl SegmentationProvider for Fake {
        fn model_version(&self) -> &str {
            "1"
        }
        fn targets(&self) -> &[SegmentTarget] {
            &[SegmentTarget::Subject]
        }
        fn load(&self, _ctx: &LoadContext) -> Result<Box<dyn Segmenter>, SegmentError> {
            if FAIL_LOADS.load(Ordering::SeqCst) > 0 {
                FAIL_LOADS.fetch_sub(1, Ordering::SeqCst);
                return Err(SegmentError::NotInstalled("still downloading".into()));
            }
            LOADS.fetch_add(1, Ordering::SeqCst);
            struct S;
            impl Segmenter for S {
                fn segment(
                    &mut self,
                    image: &ModelImage,
                    _p: &Value,
                ) -> Result<AlphaMap, SegmentError> {
                    SEGMENTS.fetch_add(1, Ordering::SeqCst);
                    AlphaMap::new(
                        image.width,
                        image.height,
                        vec![1.0; image.width * image.height],
                    )
                }
            }
            Ok(Box::new(S))
        }
    }

    fn backend() -> RegistryBackend {
        let mut r: SegmentationRegistry = Registry::new();
        r.register(
            Descriptor {
                id: "test.fake",
                schema_version: 1,
            },
            || Arc::new(Fake),
        )
        .unwrap();
        RegistryBackend::new(
            Arc::new(r),
            ModelStore::new(std::env::temp_dir().join("nicti-backend-test")),
            None,
        )
    }

    fn recipe(version: &str) -> MaskRecipe {
        MaskRecipe {
            model_id: "test.fake".into(),
            model_version: version.into(),
            params: serde_json::json!({ "target": "subject" }),
            seed: None,
        }
    }

    fn photo() -> RgbBuffer {
        RgbBuffer {
            width: 8,
            height: 6,
            data: vec![[0.2, 0.3, 0.1]; 48],
        }
    }

    // The counters are process-global, so this is one test: cargo runs tests in parallel and
    // separate tests would race on them.
    #[test]
    fn the_backend_loads_lazily_once_retries_a_failed_load_and_rejects_bad_recipes() {
        LOADS.store(0, Ordering::SeqCst);
        SEGMENTS.store(0, Ordering::SeqCst);
        FAIL_LOADS.store(0, Ordering::SeqCst);
        let mut b = backend();
        let img = photo();
        let bake = |b: &mut RegistryBackend, r: &MaskRecipe, key: u64| {
            b.bake(&BakeRequest {
                image_key: key,
                source: &img,
                cam_mul: [1.0; 4],
                recipe: r,
            })
        };
        let good = recipe("1");
        assert!(
            !b.is_loaded("test.fake", "1"),
            "nothing loads at construction"
        );

        // A failed load is an error, is not cached, and the next attempt tries again.
        FAIL_LOADS.store(1, Ordering::SeqCst);
        assert!(matches!(
            bake(&mut b, &good, 5),
            Err(SegmentError::NotInstalled(_))
        ));
        assert!(!b.is_loaded("test.fake", "1"));
        let a = bake(&mut b, &good, 5).expect("the retry succeeds once the model is there");
        assert_eq!((a.width, a.height), (1024, 768));
        assert_eq!(LOADS.load(Ordering::SeqCst), 1);

        // Later bakes reuse the loaded segmenter, for the same photo and for another.
        bake(&mut b, &good, 5).unwrap();
        bake(&mut b, &good, 6).unwrap();
        assert_eq!(LOADS.load(Ordering::SeqCst), 1, "loaded once, ever");
        assert_eq!(SEGMENTS.load(Ordering::SeqCst), 3);

        // Unknown model / stale version / nonsense target are typed errors that load nothing.
        let mut unknown = recipe("1");
        unknown.model_id = "nope".into();
        assert!(matches!(
            bake(&mut b, &unknown, 5),
            Err(SegmentError::UnknownModel(_))
        ));
        assert!(matches!(
            bake(&mut b, &recipe("0"), 5),
            Err(SegmentError::VersionMismatch { .. })
        ));
        let mut bad = recipe("1");
        bad.params = serde_json::json!({ "target": "water" });
        assert!(matches!(
            bake(&mut b, &bad, 5),
            Err(SegmentError::UnsupportedTarget(_))
        ));
        assert_eq!(LOADS.load(Ordering::SeqCst), 1);

        idle_models_are_dropped_after_the_ttl_and_reload_on_the_next_bake();
    }

    // Called from the test above rather than being its own `#[test]`: it reads the same
    // process-global load counter.
    fn idle_models_are_dropped_after_the_ttl_and_reload_on_the_next_bake() {
        let mut b = backend();
        let img = photo();
        let good = recipe("1");
        let ttl = Duration::from_secs(60);
        let t0 = Instant::now();
        assert_eq!(
            b.idle_unload_in(t0, ttl),
            None,
            "nothing loaded, nothing to unload"
        );
        assert!(!b.unload_if_idle(t0 + ttl * 2, ttl));

        let loads_before = LOADS.load(Ordering::SeqCst);
        let bake = |b: &mut RegistryBackend| {
            b.bake(&BakeRequest {
                image_key: 1,
                source: &img,
                cam_mul: [1.0; 4],
                recipe: &good,
            })
            .unwrap()
        };
        bake(&mut b);
        let used = b.last_used.expect("a bake records its time");
        assert_eq!(LOADS.load(Ordering::SeqCst), loads_before + 1);

        // Not yet due: reports the time left and keeps the model.
        let left = b.idle_unload_in(used + Duration::from_secs(20), ttl);
        assert_eq!(left, Some(Duration::from_secs(40)));
        assert!(!b.unload_if_idle(used + Duration::from_secs(59), ttl));
        assert!(b.is_loaded("test.fake", "1"));

        // Due: dropped, and the next bake loads again.
        assert!(b.unload_if_idle(used + ttl, ttl));
        assert!(!b.is_loaded("test.fake", "1"));
        assert_eq!(b.idle_unload_in(used + ttl, ttl), None);
        bake(&mut b);
        assert_eq!(LOADS.load(Ordering::SeqCst), loads_before + 2);
    }
}
