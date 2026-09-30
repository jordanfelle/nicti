//! Wires a synthetic `LinearFrame` through the real Tapetum pipeline (decode -> demosaic/denoise/
//! lens/heal passthrough -> fused live suffix -> crop), so the Develop panel (`develop_panel.rs`)
//! has a real, non-mock render to display and edit. Loading an actual NEF is #31's (loupe) scope
//! -- that needs the `libraw` feature, which this crate deliberately never forwards (`nicti-cornea`'s
//! own feature-gate doc comment: a caller wanting the real decoder enables it explicitly, this
//! crate isn't that caller yet). Graph-building shape copied from `bench/knead/src/lib.rs::build_graph`
//! (not depended on -- `bench/knead` isn't a production crate, see its own Cargo.toml).
//!
//! #46 adds real editing: an in-memory `nicti_pawprint::EditDocument` (catalog persistence of
//! edits is #31's scope, once a real asset exists to persist against -- this ticket only proves
//! the sliders actually change the render), a before/after toggle (renders with an empty document
//! so every stage falls back to its own default, reusing the exact same `apply_document` fallback
//! path a missing entry already takes), and a live histogram (a CPU readback + display-encode of
//! the current render -- cheap at this view's small synthetic extent; a throttled/GPU histogram
//! for a real full-res photo is a follow-up once #31 replaces the synthetic frame).

use std::sync::Arc;

use crate::camera_profiles::{self, ProfileEntry};
use nicti_calico::dcp::DcpProfile;
use nicti_calico::profile::ProfileSolution;
use nicti_cornea::LinearFrame;
use nicti_pawprint::{EditDocument, StageEntry};
use nicti_tapetum::coat::{
    self, CameraProfileParams, CropParams, ExposureParams, HealParams, HslParams,
    NoiseReductionParams, SharpenParams, ToneCurveParams, ToneParams, VibranceParams, WbParams,
};
use nicti_tapetum::color;
use nicti_tapetum::frame::{Extent, FrameTexture};
use nicti_tapetum::geometry::{self, output_encode};
use nicti_tapetum::gpu::GpuContext;
use nicti_tapetum::graph::{RenderGraph, StageKind, StageNode};
use nicti_tapetum::heal::{self, HealExec, HealKernel, RemovalPatch, RemovalSet};
use nicti_tapetum::histogram::{self, Histogram};
use nicti_tapetum::renderer::{BakedExec, RenderRequest, Renderer};
use nicti_tapetum::stages::{
    self, CropKernel, DecodeExec, DecodeKernel, LiveParams, LiveSuffixKernel, PassthroughExec,
    CROP, DECODE, DEMOSAIC, DENOISE, EXPOSURE, HEAL, HSL, LENS, NOISE_REDUCTION, SHARPEN, TONE,
    TONE_CURVE, VIBRANCE, WB, WORKING_SPACE,
};
use nicti_tapetum::{RenderStage, StageRegistry};

const LIVE_IDS: [&str; 9] = [
    WB,
    WORKING_SPACE,
    EXPOSURE,
    TONE,
    TONE_CURVE,
    VIBRANCE,
    HSL,
    SHARPEN,
    NOISE_REDUCTION,
];

fn build_graph() -> RenderGraph {
    let mut graph = RenderGraph::new();
    let baked_ids = [DECODE, DEMOSAIC, DENOISE, LENS, HEAL];
    let mut prev: Option<&str> = None;
    for id in baked_ids {
        graph
            .add_node(StageNode {
                id: id.to_string(),
                kind: StageKind::Baked,
                upstream: prev.map(|p| vec![p.to_string()]).unwrap_or_default(),
                own_hash: blake3::hash(id.as_bytes()),
            })
            .unwrap();
        prev = Some(id);
    }
    for id in LIVE_IDS {
        graph
            .add_node(StageNode {
                id: id.to_string(),
                kind: StageKind::Live,
                upstream: vec![prev.unwrap().to_string()],
                own_hash: blake3::hash(id.as_bytes()),
            })
            .unwrap();
        prev = Some(id);
    }
    graph
        .add_node(StageNode {
            id: CROP.to_string(),
            kind: StageKind::Geometry,
            upstream: vec![prev.unwrap().to_string()],
            own_hash: blake3::hash(CROP.as_bytes()),
        })
        .unwrap();
    graph
}

macro_rules! render_stage_factory {
    ($name:ident, $stage_fn:path) => {
        fn $name() -> Arc<dyn RenderStage> {
            Arc::new($stage_fn())
        }
    };
}
render_stage_factory!(decode_factory, stages::decode_stage);
render_stage_factory!(demosaic_factory, stages::demosaic_stage);
render_stage_factory!(denoise_factory, stages::denoise_stage);
render_stage_factory!(lens_factory, stages::lens_stage);
render_stage_factory!(heal_factory, stages::heal_stage);
render_stage_factory!(wb_factory, stages::wb_stage);
render_stage_factory!(working_space_factory, stages::working_space_stage);
render_stage_factory!(exposure_factory, stages::exposure_stage);
render_stage_factory!(tone_factory, stages::tone_stage);
render_stage_factory!(tone_curve_factory, stages::tone_curve_stage);
render_stage_factory!(vibrance_factory, stages::vibrance_stage);
render_stage_factory!(hsl_factory, stages::hsl_stage);
render_stage_factory!(sharpen_factory, stages::sharpen_stage);
render_stage_factory!(noise_reduction_factory, stages::noise_reduction_stage);
render_stage_factory!(crop_factory, stages::crop_stage);

type StageFactoryEntry = (&'static str, fn() -> Arc<dyn RenderStage>);

/// Every stage this crate's own render graph (`build_graph`) can reference -- `apply_document`
/// errors on any graph node id missing from this registry, so this list must stay in sync with
/// `build_graph`'s own node ids.
fn build_registry() -> StageRegistry {
    let mut registry = StageRegistry::new();
    let entries: [StageFactoryEntry; 15] = [
        (DECODE, decode_factory),
        (DEMOSAIC, demosaic_factory),
        (DENOISE, denoise_factory),
        (LENS, lens_factory),
        (HEAL, heal_factory),
        (WB, wb_factory),
        (WORKING_SPACE, working_space_factory),
        (EXPOSURE, exposure_factory),
        (TONE, tone_factory),
        (TONE_CURVE, tone_curve_factory),
        (VIBRANCE, vibrance_factory),
        (HSL, hsl_factory),
        (SHARPEN, sharpen_factory),
        (NOISE_REDUCTION, noise_reduction_factory),
        (CROP, crop_factory),
    ];
    for (id, factory) in entries {
        registry
            .register(
                nicti_claw::Descriptor {
                    id,
                    schema_version: 1,
                },
                factory,
            )
            .expect("every stage id above is namespaced and registered exactly once");
    }
    registry
}

/// A synthetic 64x64 "RAW" gradient frame -- a real `LinearFrame`, real decode/normalize/live-
/// suffix/crop dispatches, but no real file on disk (loading one is #31's job). Distinct per-
/// channel gradients so a color-pipeline bug (e.g. a channel swap) is visible, not masked by a
/// flat test color.
fn synthetic_linear_frame() -> LinearFrame {
    const SIZE: u32 = 64;
    let black = 0u32;
    let maximum = 4095u32;
    let mut pixels = Vec::with_capacity((SIZE * SIZE * 3) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let r = (x * maximum / SIZE) as u16;
            let g = (y * maximum / SIZE) as u16;
            let b = (((x + y) * maximum) / (2 * SIZE)) as u16;
            pixels.extend_from_slice(&[r, g, b]);
        }
    }
    LinearFrame {
        make: "Nicti".to_string(),
        model: "Synthetic".to_string(),
        width: SIZE,
        height: SIZE,
        black,
        maximum,
        cam_mul: [1.0, 1.0, 1.0, 1.0],
        pre_mul: [1.0, 1.0, 1.0, 1.0],
        cam_xyz: [
            1.0, 0.0, 0.0, // R row
            0.0, 1.0, 0.0, // G row
            0.0, 0.0, 1.0, // B row
            0.0, 0.0, 0.0, // unused G2 row
        ],
        cblack: [0, 0, 0, 0],
        pixels,
    }
}

/// Owns the long-lived kernels and `Renderer` a real develop view needs -- built once
/// (`DecodeKernel`/`LiveSuffixKernel`/`CropKernel` compile a pipeline apiece; rebuilding one per
/// render measured ~1000x too slow in this repo's own prior research, see
/// `nicti_tapetum::gpu::make_compute_pipeline`'s own doc comment) and reused across every render.
pub struct DevelopView {
    gpu: Arc<GpuContext>,
    /// `Arc`, not owned -- #31 phase 3's `load_real_frame` swaps this on every loupe cursor move,
    /// and a real decoded photo's pixel buffer is large enough (hundreds of MB at full res) that
    /// cloning it on every swap would be a real cost, not just style.
    frame: Arc<LinearFrame>,
    extent: Extent,
    graph: RenderGraph,
    registry: StageRegistry,
    /// The user's actual edits. Not yet persisted to a catalog (#31's scope, once a real asset
    /// exists) -- lives only for this session/view's lifetime.
    document: EditDocument,
    decode_kernel: DecodeKernel,
    live_kernel: LiveSuffixKernel,
    crop_kernel: CropKernel,
    heal_kernel: HealKernel,
    /// Finished AI removals for the current photo, keyed by `heal::spot_key`. Cleared whenever the
    /// photo changes: a patch is pixels inpainted from *this* frame and means nothing on another.
    removals: RemovalSet,
    /// Identity of the loaded photo as a `u64`, keying the removal engine's per-photo caches (its
    /// model frame and SAM embedding). Changes whenever `load_real_frame` swaps the photo.
    frame_key: u64,
    /// When set, `render` shows the whole frame with no crop/straighten applied. The heal tool
    /// turns this on so on-image spot positions map to source pixels by a plain stretch, with no
    /// inverse crop transform in the way.
    pub uncropped_preview: bool,
    renderer: Renderer,
    /// When true, `render()` renders with every stage at its default instead of `document`'s own
    /// values -- the before/after toggle.
    pub show_before: bool,
    /// DCP profiles available for the current frame's camera (#42), rediscovered on every
    /// `load_real_frame`. Empty for the synthetic frame or a camera with no installed profiles.
    profile_choices: Vec<ProfileEntry>,
    /// The camera the choices were discovered for, so navigating between photos from the same
    /// camera doesn't re-walk the profile folders (thousands of files) on every cursor move.
    profiles_for: Option<Vec<String>>,
    /// The parsed profile the document's `CameraProfileParams` refers to. Kept out of the
    /// document itself (it is large and not JSON); the document holds only its identity.
    active_profile: Option<Arc<DcpProfile>>,
    /// The last profile load failure, for the picker to show.
    pub profile_error: Option<String>,
}

impl DevelopView {
    pub fn new(gpu: Arc<GpuContext>) -> Self {
        let frame = Arc::new(synthetic_linear_frame());
        let extent = Extent {
            width: frame.width,
            height: frame.height,
        };
        let decode_kernel = DecodeKernel::new(&gpu);
        let live_kernel = LiveSuffixKernel::new(&gpu);
        let crop_kernel = CropKernel::new(&gpu);
        crop_kernel.set_transform(geometry::Affine2D::IDENTITY);
        let heal_kernel = HealKernel::new(&gpu);
        let renderer = Renderer::new(Arc::clone(&gpu), 500_000_000);

        Self {
            gpu,
            frame,
            extent,
            graph: build_graph(),
            registry: build_registry(),
            document: EditDocument::default(),
            decode_kernel,
            live_kernel,
            crop_kernel,
            heal_kernel,
            removals: RemovalSet::new(),
            frame_key: 0,
            uncropped_preview: false,
            renderer,
            show_before: false,
            profile_choices: Vec::new(),
            profiles_for: None,
            active_profile: None,
            profile_error: None,
        }
    }

    /// The DCP profiles installed for the current frame's camera.
    pub fn profile_choices(&self) -> &[ProfileEntry] {
        &self.profile_choices
    }

    /// The document's currently selected camera profile (`name: None` = none).
    pub fn camera_profile(&self) -> CameraProfileParams {
        self.stage_params(WORKING_SPACE)
    }

    /// Selects `entry` (or clears the selection with `None`). Loads and parses the file; on
    /// failure leaves the previous selection untouched and records the reason in
    /// [`Self::profile_error`].
    pub fn select_camera_profile(&mut self, entry: Option<&ProfileEntry>) {
        self.profile_error = None;
        let Some(entry) = entry else {
            self.active_profile = None;
            self.reset_stage(WORKING_SPACE);
            return;
        };
        let needles = camera_profiles::camera_needles(&self.frame.make, &self.frame.model);
        match camera_profiles::load(&entry.path, Some(&needles)) {
            Ok(loaded) => {
                self.set_stage_params(
                    WORKING_SPACE,
                    &CameraProfileParams {
                        name: Some(loaded.profile.name.clone()),
                        path: Some(loaded.path.display().to_string()),
                        content_hash: Some(loaded.content_hash),
                    },
                );
                self.active_profile = Some(loaded.profile);
            }
            Err(e) => self.profile_error = Some(e),
        }
    }

    /// Reads a stage's current typed params -- `document`'s own entry if present, else the
    /// registered stage's own `default_params()`. Never affected by `show_before` (that only
    /// changes what `render()` itself uses); a UI slider always reflects the real edit, not
    /// whatever the before/after toggle happens to show right now.
    pub fn stage_params<T: serde::de::DeserializeOwned + Default>(&self, stage_id: &str) -> T {
        match self.document.stages.get(stage_id) {
            Some(entry) => coat::parse(&entry.params),
            None => T::default(),
        }
    }

    /// Sets a stage's params from a typed value, replacing any existing entry -- the write half
    /// of [`Self::stage_params`].
    pub fn set_stage_params<T: serde::Serialize>(&mut self, stage_id: &str, params: &T) {
        let value = serde_json::to_value(params).expect("a coat params struct always serializes");
        self.document.stages.insert(
            stage_id.to_string(),
            StageEntry {
                schema_version: 1,
                params: value,
            },
        );
    }

    /// Removes a stage's entry entirely, reverting it to its own default -- what a slider's
    /// double-click-to-reset gesture calls.
    pub fn reset_stage(&mut self, stage_id: &str) {
        self.document.stages.remove(stage_id);
    }

    fn resolve<T: serde::de::DeserializeOwned + Default>(doc: &EditDocument, id: &str) -> T {
        match doc.stages.get(id) {
            Some(entry) => coat::parse(&entry.params),
            None => T::default(),
        }
    }

    /// Renders the current frame at its native extent, returning the final (post-crop) texture.
    /// Uses `document`'s edits, unless [`Self::show_before`] is set, in which case every stage
    /// renders at its default -- the same "no entry -> default" fallback `apply_document` already
    /// gives a document with no entry for a stage, just applied to the whole document at once.
    pub fn render(&mut self) -> Arc<FrameTexture> {
        let empty;
        let stamped;
        let doc = if self.show_before {
            empty = EditDocument::default();
            &empty
        } else {
            // The heal entry is stamped with which AI removals are ready, so a patch arriving (or
            // changing) rebakes the heal stage through the normal cache-key path.
            let mut d = self.document.clone();
            heal::stamp_removal_state(&mut d, &self.removals);
            if self.uncropped_preview {
                // Removing the entry (rather than only ignoring it below) also gives the crop node
                // its default hash, so the cached cropped composite can't be served back.
                d.stages.remove(CROP);
            }
            stamped = d;
            &stamped
        };
        self.graph
            .apply_document(doc, &self.registry)
            .expect("build_registry covers every id build_graph adds");

        let wb: WbParams = Self::resolve(doc, WB);
        let exposure: ExposureParams = Self::resolve(doc, EXPOSURE);
        let tone: ToneParams = Self::resolve(doc, TONE);
        let tone_curve: ToneCurveParams = Self::resolve(doc, TONE_CURVE);
        let vibrance: VibranceParams = Self::resolve(doc, VIBRANCE);
        let hsl: HslParams = Self::resolve(doc, HSL);
        let sharpen: SharpenParams = Self::resolve(doc, SHARPEN);
        let noise_reduction: NoiseReductionParams = Self::resolve(doc, NOISE_REDUCTION);
        let crop: CropParams = Self::resolve(doc, CROP);

        let source_extent = (self.extent.width as f32, self.extent.height as f32);
        let rect = crop.effective_rect(source_extent);
        let transform = geometry::affine_for_crop(rect, crop.rotation_degrees);
        self.crop_kernel.set_transform(transform);

        // A selected DCP profile (#42) replaces the plain LibRaw matrix with its own
        // illuminant-interpolated matrix (WB folded in per the DNG ForwardMatrix contract) and
        // adds the HueSatMap / baseline exposure / LookTable stages. `doc` is empty under
        // `show_before`, so the before view is always the plain matrix.
        let chosen: CameraProfileParams = Self::resolve(doc, WORKING_SPACE);
        let solution: Option<Arc<ProfileSolution>> =
            match (&chosen.content_hash, &self.active_profile) {
                (Some(_), Some(profile)) => {
                    let gains =
                        color::wb_gains_with_params(self.frame.cam_mul, &self.frame.cam_xyz, &wb);
                    Some(Arc::new(profile.solve(gains.map(f64::from))))
                }
                _ => None,
            };
        let matrix = match &solution {
            Some(s) => s.camera_to_working,
            None => {
                color::camera_to_working_space_matrix(self.frame.cam_mul, &self.frame.cam_xyz, &wb)
            }
        };
        self.live_kernel.set_params(
            &self.gpu,
            &LiveParams {
                working_space_matrix: matrix,
                camera_profile: solution,
                exposure,
                tone,
                tone_curve,
                vibrance,
                hsl,
                sharpen,
                noise_reduction,
                pixel_scale: 1.0,
            },
        );

        let decode_exec = DecodeExec {
            kernel: &self.decode_kernel,
            frame: &self.frame,
        };
        let heal: HealParams = Self::resolve(doc, HEAL);
        let heal_exec = HealExec {
            kernel: &self.heal_kernel,
            params: &heal,
            removals: &self.removals,
        };
        let passthrough = PassthroughExec;
        let baked_chain: Vec<(&str, &dyn BakedExec)> = vec![
            (DECODE, &decode_exec),
            (DEMOSAIC, &passthrough),
            (DENOISE, &passthrough),
            (LENS, &passthrough),
            (HEAL, &heal_exec),
        ];
        let req = RenderRequest {
            graph: &self.graph,
            baked_chain: &baked_chain,
            live: &self.live_kernel,
            live_nodes: &LIVE_IDS,
            geometry: &self.crop_kernel,
            geometry_nodes: &[CROP],
            extent: self.extent,
        };
        self.renderer
            .render(&req)
            .expect("the synthetic frame's own graph/extent are always internally consistent")
    }

    /// A live histogram of `frame`'s display-encoded pixels -- a CPU readback, cheap at this
    /// view's small synthetic extent (see this module's own doc comment for why a full-res photo
    /// would need a different, throttled/GPU approach instead).
    pub fn histogram(&self, frame: &FrameTexture) -> Histogram {
        let pixels = nicti_tapetum::frame::read_frame(&self.gpu, frame);
        let display: Vec<[f32; 4]> = pixels
            .iter()
            .map(|p| {
                let encoded = output_encode([p[0], p[1], p[2]]);
                [encoded[0], encoded[1], encoded[2], p[3]]
            })
            .collect();
        histogram::from_display_pixels(&display)
    }

    /// One-click Auto tone (#46): renders at default params (the same image ADR-0099 analyzes),
    /// histograms it, and writes `nicti_tapetum::perk::estimate`'s Exposure/Basic-tone output into
    /// `document` -- see `perk.rs`'s own doc comment for why this is provisional (candidate A,
    /// pending #202). Per ADR-0101 an unchanged result must not create a history step (#312), and
    /// a low-confidence result is applied with a marker (#311).
    pub fn apply_auto_tone(&mut self) {
        let was_before = self.show_before;
        self.show_before = true;
        let default_render = self.render();
        let hist = self.histogram(&default_render);
        self.show_before = was_before;

        let (exposure, tone) = nicti_tapetum::perk::estimate(&hist);
        self.set_stage_params(EXPOSURE, &exposure);
        self.set_stage_params(TONE, &tone);
    }

    /// Whether `document` holds any real edit at all -- what a caller (the Loupe view, #31 phase
    /// 3) checks before calling [`Self::load_real_frame`], so switching to a different photo in
    /// Loupe doesn't silently discard an unsaved edit session open on the Develop tab for
    /// whatever's currently loaded. Catalog persistence of edits is a separate, still-open
    /// follow-up (see this file's own module doc comment) -- this only guards against *losing*
    /// an in-memory edit to an unrelated navigation action, not against it never being saved at
    /// all.
    pub fn has_edits(&self) -> bool {
        !self.document.stages.is_empty()
    }

    /// Loads a real decoded photo (#31 phase 3) in place of whatever frame is currently showing,
    /// resetting `document` to a fresh default -- edits aren't persisted across a navigation
    /// change yet (catalog persistence of edits is a documented follow-up, not this ticket's
    /// scope, per this file's own module doc comment) -- and updating the `DECODE` stage's
    /// `own_hash` to `identity` so Tapetum's baked-output cache doesn't collide between different
    /// real photos at the same pixel extent. `identity` is the caller's job to compute
    /// (`crate::loupe::asset_cache_key`) -- this crate stays decoupled from `nicti-lair`. Callers
    /// should check [`Self::has_edits`] first if silently discarding an active edit session would
    /// be a surprise (the Loupe view does -- see its own caller-side guard).
    pub fn load_real_frame(&mut self, frame: Arc<LinearFrame>, identity: blake3::Hash) {
        self.extent = Extent {
            width: frame.width,
            height: frame.height,
        };
        self.frame = frame;
        self.document = EditDocument::default();
        self.removals.clear();
        self.frame_key = u64::from_le_bytes(identity.as_bytes()[..8].try_into().expect("8 bytes"));
        self.show_before = false;
        self.active_profile = None;
        self.profile_error = None;
        let needles = camera_profiles::camera_needles(&self.frame.make, &self.frame.model);
        if self.profiles_for.as_ref() != Some(&needles) {
            self.profile_choices = camera_profiles::discover(&self.frame.make, &self.frame.model);
            self.profiles_for = Some(needles);
        }
        self.graph
            .set_own_hash(DECODE, identity)
            .expect("DECODE is always present -- build_graph always adds it");
    }

    /// The loaded frame, shared (a full-resolution frame is hundreds of MB; never clone the pixels).
    pub fn frame_arc(&self) -> Arc<LinearFrame> {
        Arc::clone(&self.frame)
    }

    /// Cache key for the loaded photo (see the `frame_key` field).
    pub fn frame_key(&self) -> u64 {
        self.frame_key
    }

    /// Records a finished AI removal for `spot_key` (see `heal::spot_key`); the next `render`
    /// rebakes the heal stage with it. Replaces any earlier patch for the same spot.
    pub fn set_removal(&mut self, spot_key: String, patch: Arc<RemovalPatch>) {
        self.removals.insert(spot_key, patch);
    }

    /// Drops removal patches whose spot is no longer in the document's heal entry, so an edited
    /// or deleted spot's stale fill can't linger (or be re-stamped into the cache key).
    pub fn prune_removals(&mut self) {
        let live: std::collections::HashSet<String> = self
            .stage_params::<HealParams>(HEAL)
            .spots
            .iter()
            .map(heal::spot_key)
            .collect();
        self.removals.retain(|k, _| live.contains(k));
    }

    /// The source frame's own extent, in pixels -- what a crop/straighten UI needs to map a
    /// normalized overlay rect (or a viewport drag position) into `CropParams`'s own source-pixel
    /// space.
    pub fn source_extent(&self) -> (f32, f32) {
        (self.extent.width as f32, self.extent.height as f32)
    }

    /// The Ctrl-drag-a-reference-line gesture (#47): given a drag vector `(dx, dy)` in the *same*
    /// pixel space the current render is displayed in, computes the correcting rotation and adds
    /// it to the crop's current `rotation_degrees` (clamped). Distinct from
    /// [`Self::apply_auto_straighten`] (the Canny/Hough button) -- both write into the same
    /// `CropParams::rotation_degrees` field, since they're complementary entry points to the same
    /// value, not alternates with separate storage. A zero-length drag is a documented no-op
    /// (`geometry::straighten_delta_degrees` itself returns `0.0` for one).
    pub fn straighten_from_drag(&mut self, dx: f32, dy: f32) {
        let delta = geometry::straighten_delta_degrees(dx, dy);
        let mut crop: CropParams = self.stage_params(CROP);
        crop.set_rotation(crop.rotation_degrees + delta);
        self.set_stage_params(CROP, &crop);
    }

    /// Auto-level (#47): renders the image with crop/straighten reset to identity (so detection
    /// isn't biased by any rotation already applied), runs Canny+Hough on that render, and -- if a
    /// confident near-horizontal/near-vertical line was found -- **sets** the crop's
    /// `rotation_degrees` (clamped) to the detected correction. Unlike
    /// [`Self::straighten_from_drag`] (which is inherently relative -- a drag only ever describes
    /// a delta from wherever the crop already is), the detected angle here is measured against the
    /// identity-rotation render, so it's already the absolute angle that levels the image; adding
    /// it to whatever `rotation_degrees` already held would double-apply any rotation the user had
    /// already dialed in. A no-op (leaves `rotation_degrees` untouched) if no confident line was
    /// detected -- see `nicti_tapetum::autolevel::detect_level_angle`'s own doc comment for when
    /// that happens. Per ADR-0101 (#311) this will also surface a non-modal hint (distinct
    /// wording for no-result vs. low-confidence) and skip, not apply, a low-confidence angle.
    pub fn apply_auto_straighten(&mut self) {
        let had_crop = self.document.stages.remove(CROP);
        let was_before = self.show_before;
        self.show_before = false;
        let uncropped = self.render();
        self.show_before = was_before;
        if let Some(entry) = had_crop {
            self.document.stages.insert(CROP.to_string(), entry);
        }

        let pixels = nicti_tapetum::frame::read_frame(&self.gpu, &uncropped);
        let display: Vec<[f32; 4]> = pixels
            .iter()
            .map(|p| {
                let encoded = output_encode([p[0], p[1], p[2]]);
                [encoded[0], encoded[1], encoded[2], p[3]]
            })
            .collect();

        let Some(delta) = nicti_tapetum::autolevel::detect_level_angle(
            &display,
            uncropped.extent.width,
            uncropped.extent.height,
        ) else {
            return;
        };
        let mut crop: CropParams = self.stage_params(CROP);
        crop.set_rotation(delta);
        self.set_stage_params(CROP, &crop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Regression test for a real data-loss bug caught in this ticket's own adversarial review:
    /// an earlier version of the Loupe view (#31 phase 3) would call `load_real_frame`
    /// unconditionally, silently discarding whatever edits were open on the Develop tab the
    /// moment a different photo's decode landed in the shared `DevelopView`. `has_edits` is what
    /// the Loupe view's own caller-side guard checks before doing that -- this proves it actually
    /// reflects `document`'s real state, not just that it compiles.
    #[test]
    fn has_edits_reflects_the_document_s_real_state() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        assert!(!view.has_edits(), "a fresh DevelopView has no edits yet");

        let mut exposure: ExposureParams = view.stage_params(EXPOSURE);
        exposure.stops = 1.0;
        view.set_stage_params(EXPOSURE, &exposure);
        assert!(view.has_edits(), "a real stage entry was just set");

        view.reset_stage(EXPOSURE);
        assert!(!view.has_edits(), "the only edit was just reset away");
    }

    /// Selecting a camera profile must (a) record its identity in the edit document, (b) change
    /// the rendered pixels, (c) round-trip cleanly back to the plain matrix when cleared, and (d)
    /// re-render when a *different* profile is selected -- the live output cache is keyed on the
    /// profile's content hash, so a stale hit would show the old profile's pixels.
    #[test]
    fn selecting_a_camera_profile_changes_the_render_and_clearing_restores_it() {
        use nicti_tapetum::frame::read_frame;
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));

        // The synthetic frame is make "Nicti", model "Synthetic".
        let dir = tempfile::tempdir().unwrap();
        let write = |file: &str, scale: f32| {
            let path = dir.path().join(file);
            std::fs::write(
                &path,
                nicti_calico::dcp::testing::synthetic_dcp_bytes(
                    "NICTI SYNTHETIC",
                    file,
                    Some(scale),
                    None,
                    true,
                ),
            )
            .unwrap();
            ProfileEntry {
                name: file.to_string(),
                path,
            }
        };
        let bright = write("Bright", 1.6);
        let dark = write("Dark", 0.6);

        let pixels = |view: &mut DevelopView| {
            let frame = view.render();
            read_frame(&gpu, &frame)
        };
        let plain = pixels(&mut view);

        view.select_camera_profile(Some(&bright));
        assert!(view.profile_error.is_none(), "{:?}", view.profile_error);
        let chosen = view.camera_profile();
        assert_eq!(chosen.name.as_deref(), Some("Bright"));
        assert_eq!(chosen.content_hash.as_ref().map(String::len), Some(64));
        assert!(view.has_edits());
        let with_bright = pixels(&mut view);
        assert_ne!(plain, with_bright, "the profile should change the render");

        view.select_camera_profile(Some(&dark));
        let with_dark = pixels(&mut view);
        assert_ne!(
            with_bright, with_dark,
            "a different profile must not reuse cached output"
        );

        // The before/after toggle shows the plain matrix regardless of the selection.
        view.show_before = true;
        assert_eq!(pixels(&mut view), plain);
        view.show_before = false;

        view.select_camera_profile(None);
        assert!(
            !view.has_edits(),
            "clearing the profile removes its document entry"
        );
        assert_eq!(
            pixels(&mut view),
            plain,
            "clearing must restore the plain result"
        );
    }

    #[test]
    fn a_profile_for_another_camera_is_rejected_and_leaves_the_selection_alone() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Wrong.dcp");
        std::fs::write(
            &path,
            nicti_calico::dcp::testing::synthetic_dcp_bytes("NIKON Z 8", "Wrong", None, None, true),
        )
        .unwrap();
        view.select_camera_profile(Some(&ProfileEntry {
            name: "Wrong".into(),
            path,
        }));
        assert!(view
            .profile_error
            .as_deref()
            .is_some_and(|e| e.contains("not")));
        assert!(
            !view.has_edits(),
            "a rejected profile must not touch the document"
        );
    }
}
