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

use crate::camera_profiles::{self, LookEntry, ProfileEntry};
use nicti_calico::dcp::DcpProfile;
use nicti_cornea::LinearFrame;
use nicti_pawprint::history::History;
use nicti_pawprint::{EditDocument, StageEntry};
use nicti_tapetum::auto::{AutoOutcome, AutoReason};
use nicti_tapetum::coat::{self, CameraProfileParams, CropParams, HealParams};
use nicti_tapetum::frame::{Extent, FrameTexture};
use nicti_tapetum::geometry::{self, output_encode};
use nicti_tapetum::gpu::GpuContext;
use nicti_tapetum::graph::RenderGraph;
use nicti_tapetum::heal::{self, HealExec, HealKernel, RemovalPatch, RemovalSet};
use nicti_tapetum::histogram::{self, Histogram};
use nicti_tapetum::mask::compose as mask_compose;
use nicti_tapetum::mask::engine::{AiAlpha, MaskEngine, MaskInputs};
use nicti_tapetum::mask::params::MaskParams;
use nicti_tapetum::renderer::{BakedExec, RenderRequest, Renderer};
use nicti_tapetum::slit::{LensExec, LensKernel};
use nicti_tapetum::spine::{self, build_graph, build_registry, GEOMETRY_IDS, LIVE_IDS};
use nicti_tapetum::stages::{
    CropKernel, DecodeExec, DecodeKernel, LiveSuffixKernel, PassthroughExec, CROP, DECODE,
    DEMOSAIC, DENOISE, EXPOSURE, HEAL, LENS, MASKS, NEUTRAL, TONE, WORKING_SPACE,
};
use nicti_tapetum::StageRegistry;

/// A synthetic 64x64 "RAW" gradient frame -- a real `LinearFrame`, real decode/normalize/live-
/// suffix/crop dispatches, but no real file on disk (loading one is #31's job). Distinct per-
/// channel gradients so a color-pipeline bug (e.g. a channel swap) is visible, not masked by a
/// flat test color.
pub(crate) fn synthetic_linear_frame() -> LinearFrame {
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
        dng_opcode_list3: None,
        nikon_lens_info: None,
    }
}

/// Writes to one stage closer together than this are one undo step (a slider or brush drag).
const GESTURE_WINDOW_MS: u128 = 600;

/// The GPU-free half of the Develop view: the loaded frame, the user's edit document, the render
/// graph/registry (pure data, hashing only), and everything the Develop panel reads or writes
/// (stage params, camera/look profiles, finished AI removals and alphas). Constructible and fully
/// usable with no wgpu adapter, which is what lets the Develop panel run under the headless UI
/// harness (#426); [`DevelopEngine`] is the half that actually owns GPU resources.
pub struct DevelopDoc {
    /// `Arc`, not owned -- #31 phase 3's `load_real_frame` swaps this on every loupe cursor move,
    /// and a real decoded photo's pixel buffer is large enough (hundreds of MB at full res) that
    /// cloning it on every swap would be a real cost, not just style.
    frame: Arc<LinearFrame>,
    extent: Extent,
    graph: RenderGraph,
    registry: StageRegistry,
    /// The user's actual edits and their undo/redo log (#324). The document is persisted to the
    /// catalog by the app (`PeltApp::save_develop_edits`, #57) whenever [`Self::is_dirty`]; the log
    /// is session-local and starts empty each time a photo is loaded, so Undo never reaches past
    /// what the catalog held when the photo was opened.
    history: History,
    /// The document as last loaded from / saved to the catalog: what `is_dirty` compares against.
    saved: EditDocument,
    /// Finished AI removals for the current photo, keyed by `heal::spot_key`. Cleared whenever the
    /// photo changes: a patch is pixels inpainted from *this* frame and means nothing on another.
    removals: RemovalSet,
    /// Finished AI mask alphas by bake key (#49). Alphas are pixels computed from *this* photo's
    /// neutral render, so they are cleared whenever the photo changes (their keys chain from it
    /// anyway).
    ai_alphas: std::collections::HashMap<blake3::Hash, Arc<AiAlpha>>,
    /// Identity of the loaded photo (`loupe::asset_cache_key`), stamped into every render's
    /// document (`spine::stamp_source_identity`) so Tapetum's baked cache can't serve one photo's
    /// pixels for another of the same size.
    identity: blake3::Hash,
    /// Identity of the loaded photo as a `u64`, keying the removal engine's per-photo caches (its
    /// model frame and SAM embedding). Changes whenever `load_real_frame` swaps the photo.
    frame_key: u64,
    /// When set, `render` shows the whole frame with no crop/straighten applied. The heal tool
    /// turns this on so on-image spot positions map to source pixels by a plain stretch, with no
    /// inverse crop transform in the way.
    pub uncropped_preview: bool,
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
    /// Look `.xmp` profiles installed (#321); camera-independent, discovered once.
    look_choices: Vec<LookEntry>,
    looks_discovered: bool,
    /// The parsed Look the document's `CameraProfileParams.look` refers to.
    active_look: Option<Arc<nicti_calico::xmp_profile::LookProfile>>,
    /// The last profile load failure, for the picker to show.
    pub profile_error: Option<String>,
}

/// The GPU half of the Develop view: the long-lived kernels, mask engine and `Renderer`, built once
/// (`DecodeKernel`/`LiveSuffixKernel`/`CropKernel` compile a pipeline apiece; rebuilding one per
/// render measured ~1000x too slow in this repo's own prior research, see
/// `nicti_tapetum::gpu::make_compute_pipeline`'s own doc comment) and reused across every render.
/// Every method takes the [`DevelopDoc`] it renders or analyses.
pub struct DevelopEngine {
    gpu: Arc<GpuContext>,
    decode_kernel: DecodeKernel,
    live_kernel: LiveSuffixKernel,
    crop_kernel: CropKernel,
    heal_kernel: HealKernel,
    lens_kernel: LensKernel,
    /// Local-adjustment masks (#49): the engine that builds the atlas the live shader reads.
    mask_engine: MaskEngine,
    renderer: Renderer,
}

/// The app-facing Develop view: a [`DevelopDoc`] plus the [`DevelopEngine`] that renders it.
/// Derefs to the doc, so data accessors (`stage_params`, `set_stage_params`, ...) read as before;
/// the GPU operations (`render`, `histogram`, `apply_auto_*`) live here, splitting the borrow.
pub struct DevelopView {
    pub doc: DevelopDoc,
    pub engine: DevelopEngine,
}

impl std::ops::Deref for DevelopView {
    type Target = DevelopDoc;
    fn deref(&self) -> &DevelopDoc {
        &self.doc
    }
}

impl std::ops::DerefMut for DevelopView {
    fn deref_mut(&mut self) -> &mut DevelopDoc {
        &mut self.doc
    }
}

impl DevelopView {
    pub fn new(gpu: Arc<GpuContext>) -> Self {
        Self {
            doc: DevelopDoc::new(),
            engine: DevelopEngine::new(gpu),
        }
    }

    /// See [`DevelopDoc::load_real_frame`]. Forwarded (rather than reached through `Deref`) so a
    /// call like `view.load_real_frame(view.frame_arc(), ..)` keeps its two-phase borrow.
    pub fn load_real_frame(
        &mut self,
        frame: Arc<LinearFrame>,
        identity: blake3::Hash,
        doc: EditDocument,
    ) {
        self.doc.load_real_frame(frame, identity, doc);
    }

    /// Renders the current frame at its native extent, returning the final (post-crop) texture.
    /// See [`DevelopEngine::render`].
    pub fn render(&mut self) -> Arc<FrameTexture> {
        self.engine.render(&mut self.doc)
    }

    /// One-click Auto tone (#46). See [`DevelopEngine::apply_auto_tone`].
    #[cfg(test)]
    pub fn apply_auto_tone(&mut self) -> AutoApplied {
        self.engine.apply_auto_tone(&mut self.doc)
    }

    /// Auto-level (#47). See [`DevelopEngine::apply_auto_straighten`].
    #[cfg(test)]
    pub fn apply_auto_straighten(&mut self) -> AutoApplied {
        self.engine.apply_auto_straighten(&mut self.doc)
    }
}

fn has_null(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => true,
        serde_json::Value::Array(a) => a.iter().any(has_null),
        serde_json::Value::Object(o) => o.values().any(has_null),
        _ => false,
    }
}

impl DevelopDoc {
    pub fn new() -> Self {
        let frame = Arc::new(synthetic_linear_frame());
        let extent = Extent {
            width: frame.width,
            height: frame.height,
        };
        Self {
            frame,
            extent,
            graph: build_graph(),
            registry: build_registry(),
            history: History::new(EditDocument::default()),
            saved: EditDocument::default(),
            removals: RemovalSet::new(),
            ai_alphas: std::collections::HashMap::new(),
            identity: blake3::hash(b"synthetic"),
            frame_key: 0,
            uncropped_preview: false,
            show_before: false,
            profile_choices: Vec::new(),
            profiles_for: None,
            active_profile: None,
            look_choices: Vec::new(),
            looks_discovered: false,
            active_look: None,
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
            // A Look layers on a DCP, so clearing the profile clears it too.
            self.active_profile = None;
            self.active_look = None;
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
                        // Switching the base DCP keeps the chosen Look.
                        look: self.camera_profile().look,
                    },
                );
                self.active_profile = Some(loaded.profile);
            }
            Err(e) => self.profile_error = Some(e),
        }
    }

    /// The Look `.xmp` profiles installed (#321).
    pub fn look_choices(&self) -> &[LookEntry] {
        &self.look_choices
    }

    /// Selects a Look `.xmp` on top of the current camera profile (`None` clears it). A no-op
    /// without a selected DCP: a Look has no base to layer on. On failure the previous selection
    /// is untouched and [`Self::profile_error`] says why.
    pub fn select_look(&mut self, entry: Option<&LookEntry>) {
        self.profile_error = None;
        let mut params = self.camera_profile();
        if params.content_hash.is_none() {
            return;
        }
        match entry {
            None => {
                params.look = None;
                self.active_look = None;
            }
            Some(entry) => match camera_profiles::load_look(&entry.path) {
                Ok(loaded) => {
                    params.look = Some(nicti_tapetum::coat::LookRef {
                        name: loaded.look.name.clone(),
                        path: loaded.path.display().to_string(),
                        content_hash: loaded.content_hash,
                    });
                    self.active_look = Some(loaded.look);
                }
                Err(e) => {
                    self.profile_error = Some(e);
                    return;
                }
            },
        }
        self.set_stage_params(WORKING_SPACE, &params);
    }

    /// Reloads the document's Look, leaving `active_look` off and recording the reason on failure.
    fn reload_active_look(&mut self) {
        self.active_look = None;
        match camera_profiles::load_look_for_document(self.history.document()) {
            Ok(look) => self.active_look = look,
            Err(e) => {
                self.profile_error.get_or_insert(e);
            }
        }
    }

    /// Reads a stage's current typed params -- `document`'s own entry if present, else the
    /// registered stage's own `default_params()`. Never affected by `show_before` (that only
    /// changes what `render()` itself uses); a UI slider always reflects the real edit, not
    /// whatever the before/after toggle happens to show right now.
    pub fn stage_params<T: serde::de::DeserializeOwned + Default>(&self, stage_id: &str) -> T {
        match self.history.document().stages.get(stage_id) {
            Some(entry) => coat::parse(&entry.params),
            None => T::default(),
        }
    }

    /// Sets a stage's params from a typed value, replacing any existing entry -- the write half
    /// of [`Self::stage_params`]. Goes through the undo history (#324): a value equal to the
    /// *effective* current one (an absent entry counts as the stage default, ADR-0101 rule 6)
    /// writes nothing, so the panels can call this every frame without dirtying the document or
    /// creating steps; successive writes to one stage within [`GESTURE_WINDOW_MS`] are one step
    /// (a slider or brush drag).
    pub fn set_stage_params<T>(&mut self, stage_id: &str, params: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + Default + 'static,
    {
        let Some((_, entry)) = self.stage_edit(stage_id, params) else {
            return;
        };
        self.history.apply(stage_id, stage_id, entry);
        self.history.compact(GESTURE_WINDOW_MS);
    }

    /// The entry [`Self::set_stage_params`] would write for `params`, or `None` when it equals the
    /// effective current value. Lets a caller that edits several stages at once collect the real
    /// changes and commit them as one step with [`Self::apply_stage_edits`].
    pub fn stage_edit<T>(&self, stage_id: &str, params: &T) -> Option<(String, StageEntry)>
    where
        T: serde::Serialize + serde::de::DeserializeOwned + Default + 'static,
    {
        let mut value =
            serde_json::to_value(params).expect("a coat params struct always serializes");
        // A NaN serializes to JSON `null`, which the canonical stage hasher refuses, and
        // re-parsing that `null` would discard the whole document -- so scrub masks as a *typed*
        // value. Only when one is present: `sanitized()` also truncates (one document-wide brush
        // budget), and doing that on every write would permanently drop points the UI is still
        // adding to a stroke.
        if stage_id == MASKS && has_null(&value) {
            if let Some(masks) = (params as &dyn std::any::Any).downcast_ref::<MaskParams>() {
                value = serde_json::to_value(masks.sanitized()).expect("mask params serialize");
            }
        }
        let effective = serde_json::to_value(self.stage_params::<T>(stage_id))
            .expect("a coat params struct always serializes");
        (value != effective).then(|| {
            (
                stage_id.to_string(),
                StageEntry {
                    schema_version: 1,
                    params: value,
                },
            )
        })
    }

    /// Commits several stage edits (from [`Self::stage_edit`]) as a single undo step -- one Auto
    /// tone click is one step, not one per stage (ADR-0101 rule 6). Never coalesces with a drag.
    pub fn apply_stage_edits(&mut self, edits: Vec<(String, StageEntry)>) {
        if !edits.is_empty() {
            self.history.apply_group(edits);
        }
    }

    /// Removes a stage's entry entirely, reverting it to its own default -- what a slider's
    /// double-click-to-reset gesture calls. One undo step; a no-op for a stage with no entry.
    pub fn reset_stage(&mut self, stage_id: &str) {
        self.history.reset(stage_id, stage_id);
        self.history.compact(GESTURE_WINDOW_MS);
    }

    /// Whether [`Self::undo`] has a step to reverse.
    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    /// Whether [`Self::redo`] has a step to re-apply.
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Reverses the most recent edit step (a whole drag, a whole batch). `false` when there is
    /// nothing to undo. AI removal patches stay cached, so undoing a deleted spot brings its fill
    /// straight back; one that was evicted is re-run by the heal tool's poll.
    pub fn undo(&mut self) -> bool {
        let moved = self.history.undo();
        if moved {
            self.after_history_move();
        }
        moved
    }

    /// Re-applies the step [`Self::undo`] reversed. `false` when there is nothing to redo.
    pub fn redo(&mut self) -> bool {
        let moved = self.history.redo();
        if moved {
            self.after_history_move();
        }
        moved
    }

    /// An undo/redo can change the selected camera profile or Look, which live outside the
    /// document, so reload them from what the document now says.
    fn after_history_move(&mut self) {
        self.reload_profiles();
    }

    /// Reloads the camera profile and Look the current document selects, leaving them off (and
    /// recording the reason in [`Self::profile_error`]) when they can't be loaded.
    fn reload_profiles(&mut self) {
        self.active_profile = None;
        self.profile_error = None;
        match camera_profiles::load_for_document(
            self.history.document(),
            &self.frame.make,
            &self.frame.model,
        ) {
            Ok(profile) => self.active_profile = profile,
            Err(e) => self.profile_error = Some(e),
        }
        self.reload_active_look();
    }

    /// Cache key of the neutral render AI masks infer on (see `build_graph`'s `NEUTRAL` node):
    /// what a bake key chains from, so a finished alpha is only ever reused for the same photo.
    pub fn neutral_key(&self) -> blake3::Hash {
        self.graph
            .cache_key(NEUTRAL)
            .expect("build_graph always adds NEUTRAL")
    }

    /// The AI model runs the current masks need but don't have a finished alpha for yet. Only
    /// active corrections are listed (a disabled mask is not baked).
    pub fn mask_bake_requests(&self) -> Vec<mask_compose::BakeRequest> {
        let params: MaskParams = self.stage_params(MASKS);
        mask_compose::bake_requests(&params, self.neutral_key())
            .into_iter()
            .filter(|r| !self.ai_alphas.contains_key(&r.key))
            .collect()
    }

    /// Records a finished AI alpha under its bake key; the next `render` recomposes only the
    /// corrections that use it.
    pub fn set_ai_alpha(&mut self, bake_key: blake3::Hash, alpha: Arc<AiAlpha>) {
        self.ai_alphas.insert(bake_key, alpha);
    }

    /// The finished alpha for `bake_key`, if it has arrived (the overlay preview reads it).
    pub fn ai_alpha(&self, bake_key: &blake3::Hash) -> Option<Arc<AiAlpha>> {
        self.ai_alphas.get(bake_key).map(Arc::clone)
    }

    /// True once `bake_key`'s alpha has arrived.
    pub fn has_ai_alpha(&self, bake_key: &blake3::Hash) -> bool {
        self.ai_alphas.contains_key(bake_key)
    }

    /// Drops finished alphas no correction refers to any more. A mask that is merely toggled off
    /// keeps its alpha (only deleted or re-recipe'd masks release theirs).
    pub fn prune_ai_alphas(&mut self) {
        let params: MaskParams = self.stage_params(MASKS);
        let keep = mask_compose::referenced_bake_keys(&params, self.neutral_key());
        self.ai_alphas.retain(|k, _| keep.contains(k));
    }

    /// Whether the loaded frame is a complete decode: a non-empty extent and exactly three
    /// samples per pixel. `load_real_frame` only ever receives a fully decoded `LinearFrame`, so this
    /// is a defensive guard (ADR-0101 rule 7) -- an auto op must never analyse a partial buffer.
    fn frame_is_complete(&self) -> bool {
        let (w, h) = (self.frame.width as usize, self.frame.height as usize);
        w > 0 && h > 0 && self.frame.pixels.len() == w * h * 3
    }

    /// Whether `document` holds any real edit at all -- what a caller (the Loupe view, #31 phase
    /// 3) checks before calling [`Self::load_real_frame`], so switching to a different photo in
    /// Loupe doesn't silently discard an unsaved edit session open on the Develop tab for
    /// whatever's currently loaded. Catalog persistence of edits is a separate, still-open
    /// follow-up (see this file's own module doc comment) -- this only guards against *losing*
    /// an in-memory edit to an unrelated navigation action, not against it never being saved at
    /// all.
    #[cfg(test)]
    pub fn has_edits(&self) -> bool {
        !self.history.document().stages.is_empty()
    }

    /// The current edits.
    pub fn document(&self) -> &EditDocument {
        self.history.document()
    }

    /// Whether the edits differ from what the catalog has (`load_real_frame`'s `doc` or the last
    /// [`Self::mark_saved`]).
    pub fn is_dirty(&self) -> bool {
        self.history.document() != &self.saved
    }

    /// Records that the current edits are now persisted.
    pub fn mark_saved(&mut self) {
        self.saved = self.history.document().clone();
    }

    /// Swaps in `doc` as the loaded photo's edits, already persisted (#52): a paste/sync/undo wrote
    /// it to the catalog, and the autosave must not write this view's older copy back over it.
    /// Drops the removals and AI alphas `doc` no longer references (the next frame re-bakes any AI
    /// mask it does), and reloads the camera profile, which `doc` may have changed.
    pub fn replace_document(&mut self, doc: EditDocument) {
        self.saved = doc.clone();
        self.history = History::new(doc);
        self.prune_removals();
        self.prune_ai_alphas();
        self.reload_profiles();
    }

    /// Loads a real decoded photo (#31 phase 3) in place of whatever frame is currently showing,
    /// with `doc` as its edits (the catalog's stored master document, #57; pass
    /// `EditDocument::default()` for none), remembering `identity` so every render stamps it into
    /// its document (`spine::stamp_source_identity`) and Tapetum's baked-output cache can't
    /// collide between different real photos at the same pixel extent. `identity` is the caller's job to compute
    /// (`crate::loupe::asset_cache_key`) -- this crate stays decoupled from `nicti-lair`. Callers
    /// must save (or deliberately discard) a dirty document first -- see [`Self::is_dirty`].
    ///
    /// A camera profile the document selects is reloaded and verified against the hash the
    /// document recorded; on failure the profile is left off and [`Self::profile_error`] says why.
    /// AI removals' patches are not persisted: `doc` keeps their recipes, and the heal tool re-runs
    /// them from those (#324, `heal_tool::rerun_missing_removals`). The undo history starts empty.
    pub fn load_real_frame(
        &mut self,
        frame: Arc<LinearFrame>,
        identity: blake3::Hash,
        doc: EditDocument,
    ) {
        self.extent = Extent {
            width: frame.width,
            height: frame.height,
        };
        self.frame = frame;
        self.saved = doc.clone();
        self.history = History::new(doc);
        self.removals.clear();
        self.ai_alphas.clear();
        self.frame_key = u64::from_le_bytes(identity.as_bytes()[..8].try_into().expect("8 bytes"));
        self.show_before = false;
        self.reload_profiles();
        if !self.looks_discovered {
            self.look_choices = camera_profiles::discover_looks();
            self.looks_discovered = true;
        }
        let needles = camera_profiles::camera_needles(&self.frame.make, &self.frame.model);
        if self.profiles_for.as_ref() != Some(&needles) {
            self.profile_choices = camera_profiles::discover(&self.frame.make, &self.frame.model);
            self.profiles_for = Some(needles);
        }
        // `render` stamps the identity into the document it renders (see there), which is what
        // keeps the cache keys right. Apply it to the graph *now* as well, so `neutral_key()` -- and
        // therefore `mask_bake_requests()` -- is already the new photo's the moment it loads, not
        // one render late: a caller asking what to bake right after loading must never be handed a
        // key that belongs to the previous photo.
        self.identity = identity;
        let mut identity_only = EditDocument::default();
        spine::stamp_source_identity(&mut identity_only, identity);
        self.graph
            .apply_document(&identity_only, &self.registry)
            .expect("build_registry covers every id build_graph adds");
    }

    /// The loaded frame, shared (a full-resolution frame is hundreds of MB; never clone the pixels).
    pub fn frame_arc(&self) -> Arc<LinearFrame> {
        Arc::clone(&self.frame)
    }

    /// Whether the loaded photo is a DNG carrying its own lens profile (#428): the Lens Corrections
    /// section only offers the "use embedded profile" switch then.
    pub fn has_embedded_lens_profile(&self) -> bool {
        nicti_tapetum::slit::has_embedded_profile(&self.frame)
    }

    /// Whether the loaded photo is a Z-series NEF carrying Nikon's own correction data (#410): the
    /// Lens Corrections section only offers the opt-in "Nikon lens profile" switch then.
    pub fn has_nikon_lens_profile(&self) -> bool {
        nicti_tapetum::slit::has_nikon_profile(&self.frame)
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

    /// Whether a finished AI removal is held for `spot_key`.
    pub fn has_removal(&self, spot_key: &str) -> bool {
        self.removals.contains_key(spot_key)
    }

    /// Bounds the removal patches held for this photo. A patch is keyed by its spot's whole
    /// content, so a stale one can't be mistaken for a current spot -- and it is kept after its
    /// spot is edited or deleted so Undo brings the fill straight back instead of re-running a
    /// multi-second model. Only once more than twice [`MAX_SPOTS`] are held are the ones the
    /// document no longer references dropped.
    pub fn prune_removals(&mut self) {
        if self.removals.len() <= 2 * heal::MAX_SPOTS {
            return;
        }
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

    /// The displayed canvas's size in output pixels: the crop rect while idle (#272), the whole
    /// frame while the crop/heal/mask tools or Before show it uncropped. Must match what
    /// `DevelopEngine::render` allocates.
    pub fn display_extent(&self) -> (f32, f32) {
        if self.uncropped_preview || self.show_before {
            return self.source_extent();
        }
        let crop: CropParams = self.stage_params(CROP);
        let rect = crop.effective_rect(self.source_extent());
        let (src_w, src_h) = self.source_extent();
        (
            rect.width.round().clamp(1.0, src_w.max(1.0)),
            rect.height.round().clamp(1.0, src_h.max(1.0)),
        )
    }

    /// Maps a point on the displayed canvas, normalized to `0..=1` on each axis, to a normalized
    /// point in the source image -- the same output -> source transform the crop kernel samples
    /// with (`geometry::affine_for_crop`), or the identity while the uncropped frame is shown.
    pub fn display_to_source_norm(&self, n: [f32; 2]) -> [f32; 2] {
        let (src_w, src_h) = self.source_extent();
        if self.uncropped_preview || self.show_before {
            return n;
        }
        let crop: CropParams = self.stage_params(CROP);
        let rect = crop.effective_rect((src_w, src_h));
        let (dw, dh) = self.display_extent();
        let (x, y) =
            geometry::affine_for_crop(rect, crop.rotation_degrees).apply((n[0] * dw, n[1] * dh));
        [x / src_w.max(1.0), y / src_h.max(1.0)]
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
}

impl DevelopEngine {
    pub fn new(gpu: Arc<GpuContext>) -> Self {
        let decode_kernel = DecodeKernel::new(&gpu);
        let live_kernel = LiveSuffixKernel::new(&gpu);
        let crop_kernel = CropKernel::new(&gpu);
        crop_kernel.set_transform(geometry::Affine2D::IDENTITY);
        let heal_kernel = HealKernel::new(&gpu);
        let lens_kernel = LensKernel::new(&gpu);
        let mask_engine = MaskEngine::new(&gpu);
        let renderer = Renderer::new(Arc::clone(&gpu), 500_000_000);
        Self {
            gpu,
            decode_kernel,
            live_kernel,
            crop_kernel,
            heal_kernel,
            lens_kernel,
            mask_engine,
            renderer,
        }
    }

    /// Renders the current frame at its native extent, returning the final (post-crop) texture.
    /// Uses `document`'s edits, unless [`Self::show_before`] is set, in which case every stage
    /// renders at its default -- the same "no entry -> default" fallback `apply_document` already
    /// gives a document with no entry for a stage, just applied to the whole document at once.
    pub fn render(&mut self, dv: &mut DevelopDoc) -> Arc<FrameTexture> {
        let mut doc = if dv.show_before {
            EditDocument::default()
        } else {
            // The heal entry is stamped with which AI removals are ready, so a patch arriving (or
            // changing) rebakes the heal stage through the normal cache-key path.
            let mut d = dv.history.document().clone();
            heal::stamp_removal_state(&mut d, &dv.removals);
            if dv.uncropped_preview {
                // Removing the entry (rather than only ignoring it below) also gives the crop node
                // its default hash, so the cached cropped composite can't be served back.
                d.stages.remove(CROP);
                // The vignette/grain are relative to the crop: over the whole frame they would
                // only get in the way of seeing the spots being healed.
                d.stages.remove(nicti_tapetum::stages::EFFECTS);
            }
            d
        };
        spine::stamp_source_identity(&mut doc, dv.identity);
        dv.graph
            .apply_document(&doc, &dv.registry)
            .expect("build_registry covers every id build_graph adds");
        if !dv.show_before {
            // The masks entry is stamped with which AI alphas are ready, so one arriving (or being
            // replaced) recomposes only the corrections that use it. The stamp needs the neutral
            // render's key, which is known once the photo's identity is applied above -- hence the
            // second, nearly free `apply_document` (only the masks node's hash changes).
            let neutral_key = dv.neutral_key();
            let ready: std::collections::HashMap<blake3::Hash, blake3::Hash> = dv
                .ai_alphas
                .iter()
                .map(|(k, a)| (*k, a.content_hash))
                .collect();
            mask_compose::stamp_ai_alpha_state(&mut doc, neutral_key, &ready);
            dv.graph
                .apply_document(&doc, &dv.registry)
                .expect("build_registry covers every id build_graph adds");
        }
        let doc = &doc;

        let inputs = spine::resolve_inputs(
            doc,
            &dv.frame,
            dv.extent,
            dv.active_profile.as_deref(),
            dv.active_look.as_deref(),
            1.0,
        );
        self.crop_kernel.set_transform(inputs.crop_transform);
        inputs.bind_effects(&self.crop_kernel);
        self.live_kernel.set_params(&self.gpu, &inputs.live);

        let decode_exec = DecodeExec {
            kernel: &self.decode_kernel,
            frame: &dv.frame,
        };
        let heal_exec = HealExec {
            kernel: &self.heal_kernel,
            params: &inputs.heal,
            removals: &dv.removals,
        };
        let lens_exec = LensExec {
            kernel: &self.lens_kernel,
            params: &inputs.lens,
            frame: &dv.frame,
        };
        let passthrough = PassthroughExec;
        let baked_chain: Vec<(&str, &dyn BakedExec)> = vec![
            (DECODE, &decode_exec),
            (DEMOSAIC, &passthrough),
            (DENOISE, &passthrough),
            (LENS, &lens_exec),
            (HEAL, &heal_exec),
        ];
        // #272: the idle preview is sized to the crop rect; the crop tool, heal/mask tools and
        // Before show the whole frame.
        let (gw, gh) = dv.display_extent();
        let geometry_extent = Extent {
            width: gw as u32,
            height: gh as u32,
        };
        let req = RenderRequest {
            graph: &dv.graph,
            baked_chain: &baked_chain,
            live: &self.live_kernel,
            live_nodes: &LIVE_IDS,
            geometry: &self.crop_kernel,
            geometry_nodes: &GEOMETRY_IDS,
            extent: dv.extent,
            geometry_extent,
        };

        // Local corrections (#49) and global Presence (#380). The engine needs the *baked* frame (AI refines and range masks
        // follow it), which only exists once the baked chain has run and been submitted -- so bake
        // first, prepare the masks from it, bind them, and let the render below find every baked
        // stage already cached. With no active correction none of this costs anything.
        let mask_params: MaskParams = spine::resolve(doc, MASKS);
        // A global clarity/texture/dehaze (#380) needs the same baked frame and bases with no mask.
        let mask_frame =
            if mask_params.active().next().is_some() || inputs.live.presence.needs_bases() {
                let baked = self.renderer.render_baked(&req).expect(
                    "the synthetic frame's own graph/extent are always internally consistent",
                );
                let neutral_key = dv
                    .graph
                    .cache_key(NEUTRAL)
                    .expect("build_graph always adds NEUTRAL");
                let guide_key = dv
                    .graph
                    .cache_key(HEAL)
                    .expect("build_graph always adds HEAL");
                // Range masks measure the frame as shot: the as-shot matrix (no user white balance),
                // so a white-balance drag doesn't rebuild every range mask.
                let range_matrix = nicti_tapetum::color::camera_to_working_space_matrix(
                    dv.frame.cam_mul,
                    &dv.frame.cam_xyz,
                    &nicti_tapetum::coat::WbParams::default(),
                );
                self.mask_engine.prepare(
                    &self.gpu,
                    &MaskInputs {
                        params: &mask_params,
                        ai_alphas: &dv.ai_alphas,
                        neutral_key,
                        guide: &baked,
                        guide_key,
                        range_matrix,
                        presence: inputs.live.presence,
                    },
                )
            } else {
                None
            };
        self.live_kernel.set_masks(&self.gpu, mask_frame.as_ref());

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
    /// pending #202). Per ADR-0101 (#311): a result equal to the *effective* current params (an
    /// absent entry counts as its default) writes nothing and reports [`AutoApplied::Unchanged`],
    /// so a click never dirties the document or creates a history step; a
    /// low-confidence result is still applied and reported as
    /// [`AutoApplied::AppliedLowConfidence`] so the panel can mark the control.
    pub fn apply_auto_tone(&mut self, dv: &mut DevelopDoc) -> AutoApplied {
        if !dv.frame_is_complete() {
            return AutoApplied::Skipped {
                reason: AutoReason::DecodeIncomplete,
                low_confidence: false,
            };
        }
        let was_before = dv.show_before;
        dv.show_before = true;
        let default_render = self.render(dv);
        let hist = self.histogram(&default_render);
        dv.show_before = was_before;

        let outcome = nicti_tapetum::perk::estimate(&hist);
        let (exposure, tone) = match outcome {
            AutoOutcome::Confident(v) | AutoOutcome::LowConfidence(v, _) => v,
            AutoOutcome::NoResult(reason) => {
                return AutoApplied::Skipped {
                    reason,
                    low_confidence: false,
                };
            }
        };
        let current_exposure: coat::ExposureParams = dv.stage_params(EXPOSURE);
        let current_tone: coat::ToneParams = dv.stage_params(TONE);
        if exposure == current_exposure && tone == current_tone {
            return AutoApplied::Unchanged;
        }
        let edits = [
            dv.stage_edit(EXPOSURE, &exposure),
            dv.stage_edit(TONE, &tone),
        ];
        dv.apply_stage_edits(edits.into_iter().flatten().collect());
        match outcome {
            AutoOutcome::LowConfidence(_, reason) => AutoApplied::AppliedLowConfidence(reason),
            _ => AutoApplied::Applied,
        }
    }

    /// Auto-level (#47): renders the image with crop/straighten reset to identity (so detection
    /// isn't biased by any rotation already applied), runs Canny+Hough on that render, and -- if a
    /// confident near-horizontal/near-vertical line was found -- **sets** the crop's
    /// `rotation_degrees` (clamped) to the detected correction. Unlike
    /// [`DevelopDoc::straighten_from_drag`] (which is inherently relative -- a drag only ever describes
    /// a delta from wherever the crop already is), the detected angle here is measured against the
    /// identity-rotation render, so it's already the absolute angle that levels the image; adding
    /// it to whatever `rotation_degrees` already held would double-apply any rotation the user had
    /// already dialed in. Per ADR-0101 (#311) nothing is written (and the result says why) when no
    /// line qualified ([`AutoApplied::Skipped`], `low_confidence: false`), when the lines were too
    /// weak or inconsistent to trust (`low_confidence: true` -- a wrong rotation is worse than
    /// none), or when the angle already matches the current rotation ([`AutoApplied::Unchanged`]) --
    /// see `nicti_tapetum::autolevel::detect_level_angle`'s own doc comment.
    pub fn apply_auto_straighten(&mut self, dv: &mut DevelopDoc) -> AutoApplied {
        if !dv.frame_is_complete() {
            return AutoApplied::Skipped {
                reason: AutoReason::DecodeIncomplete,
                low_confidence: false,
            };
        }
        let (was_before, was_uncropped) = (dv.show_before, dv.uncropped_preview);
        dv.show_before = false;
        dv.uncropped_preview = true;
        let uncropped = self.render(dv);
        dv.show_before = was_before;
        dv.uncropped_preview = was_uncropped;

        let pixels = nicti_tapetum::frame::read_frame(&self.gpu, &uncropped);
        let display: Vec<[f32; 4]> = pixels
            .iter()
            .map(|p| {
                let encoded = output_encode([p[0], p[1], p[2]]);
                [encoded[0], encoded[1], encoded[2], p[3]]
            })
            .collect();

        let delta = match nicti_tapetum::autolevel::detect_level_angle(
            &display,
            uncropped.extent.width,
            uncropped.extent.height,
        ) {
            AutoOutcome::Confident(delta) => delta,
            AutoOutcome::LowConfidence(_, reason) => {
                return AutoApplied::Skipped {
                    reason,
                    low_confidence: true,
                };
            }
            AutoOutcome::NoResult(reason) => {
                return AutoApplied::Skipped {
                    reason,
                    low_confidence: false,
                };
            }
        };
        let mut crop: CropParams = dv.stage_params(CROP);
        let before = crop.rotation_degrees;
        crop.set_rotation(delta);
        if (crop.rotation_degrees - before).abs() < UNCHANGED_ROTATION_EPSILON_DEGREES {
            return AutoApplied::Unchanged;
        }
        dv.set_stage_params(CROP, &crop);
        AutoApplied::Applied
    }
}

/// What an automatic develop operation did to the document (ADR-0101), for the panel to turn into a
/// hint or marker.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AutoApplied {
    /// Written to the document with no caveat.
    Applied,
    /// Written, but the estimate was uncertain -- the control gets a marker.
    AppliedLowConfidence(AutoReason),
    /// The result equals the current parameters, so nothing was written.
    Unchanged,
    /// Nothing written. `low_confidence` distinguishes "found something too weak to trust" from
    /// "found nothing" -- the two get different hints.
    Skipped {
        reason: AutoReason,
        low_confidence: bool,
    },
}

/// A detected straighten angle within this many degrees of the current rotation counts as already
/// level: below any visible change, and absorbs f32 noise between two runs on the same pixels.
const UNCHANGED_ROTATION_EPSILON_DEGREES: f32 = 0.01;

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_tapetum::coat::{ExposureParams, ToneParams, WbParams};
    use nicti_tapetum::stages::WB;
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

    /// Regression: `apply_document` used to reset the identity `load_real_frame` set on the DECODE
    /// node, so a second photo of the same size was served the first photo's cached pixels.
    #[test]
    fn two_photos_of_the_same_size_never_share_cached_pixels() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        let a = synthetic_linear_frame();
        let mut b = synthetic_linear_frame();
        b.pixels.reverse();
        assert_eq!((a.width, a.height), (b.width, b.height));

        view.load_real_frame(Arc::new(a), blake3::hash(b"a"), EditDocument::default());
        let first = nicti_tapetum::frame::read_frame(&gpu, &view.render());
        view.load_real_frame(Arc::new(b), blake3::hash(b"b"), EditDocument::default());
        let second = nicti_tapetum::frame::read_frame(&gpu, &view.render());
        assert_ne!(
            first, second,
            "the second photo rendered the first photo's pixels"
        );
    }

    /// #57: edits are dirty relative to what the catalog holds, and loading a photo with its stored
    /// document starts clean with exactly that document.
    #[test]
    fn dirty_tracking_and_loading_a_stored_document() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        assert!(!view.is_dirty());

        let mut exposure: ExposureParams = view.stage_params(EXPOSURE);
        exposure.stops = 0.75;
        view.set_stage_params(EXPOSURE, &exposure);
        assert!(view.is_dirty(), "an edit not yet saved");
        let edited = view.document().clone();
        view.mark_saved();
        assert!(!view.is_dirty());

        // Undoing back to the empty document is a change relative to what was saved.
        view.reset_stage(EXPOSURE);
        assert!(view.is_dirty());

        // Loading another photo with a stored document: clean, and the document is exactly it.
        view.load_real_frame(view.frame_arc(), blake3::hash(b"photo"), edited.clone());
        assert!(!view.is_dirty());
        assert_eq!(view.document(), &edited);
        let loaded: ExposureParams = view.stage_params(EXPOSURE);
        assert_eq!(loaded.stops, 0.75);

        // And a photo with no stored edits loads empty.
        view.load_real_frame(
            view.frame_arc(),
            blake3::hash(b"other"),
            EditDocument::default(),
        );
        assert!(!view.is_dirty() && !view.has_edits());
    }

    /// A stored document whose camera profile is gone must not silently render with the plain
    /// matrix: Develop reports why.
    #[test]
    fn a_stored_profile_that_cannot_be_reloaded_is_reported() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        let mut doc = EditDocument::default();
        doc.stages.insert(
            WORKING_SPACE.to_string(),
            StageEntry {
                schema_version: 1,
                params: serde_json::to_value(CameraProfileParams {
                    name: Some("Gone".into()),
                    path: Some("/definitely/not/here.dcp".into()),
                    content_hash: Some("00".repeat(32)),
                    look: None,
                })
                .unwrap(),
            },
        );
        view.load_real_frame(view.frame_arc(), blake3::hash(b"x"), doc);
        assert!(view.profile_error.is_some());
        assert!(!view.is_dirty(), "loading never marks the view dirty");
    }

    /// A Look `.xmp` (#321) layers on the selected DCP: selecting one records its identity, changes
    /// the render, survives a document reload, and a file changed on disk is reported rather than
    /// silently rendering other colours. Without a DCP the selection is a no-op.
    #[test]
    fn selecting_a_look_layers_on_the_profile_and_survives_a_reload() {
        use nicti_tapetum::frame::read_frame;
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        let dir = tempfile::tempdir().unwrap();
        let dcp_path = dir.path().join("Base.dcp");
        std::fs::write(
            &dcp_path,
            nicti_calico::dcp::testing::synthetic_dcp_bytes(
                "NICTI SYNTHETIC",
                "Base",
                Some(1.2),
                None,
                true,
            ),
        )
        .unwrap();
        let base = ProfileEntry {
            name: "Base".into(),
            path: dcp_path,
        };
        let look_path = dir.path().join("Vivid.xmp");
        std::fs::write(
            &look_path,
            nicti_calico::xmp_profile::testing::synthetic_valid_xmp("Vivid"),
        )
        .unwrap();
        let look = LookEntry {
            name: "Vivid".into(),
            path: look_path.clone(),
        };

        view.select_look(Some(&look));
        assert!(
            view.camera_profile().look.is_none(),
            "a Look needs a DCP to layer on"
        );

        view.select_camera_profile(Some(&base));
        let pixels = |view: &mut DevelopView| {
            let frame = view.render();
            read_frame(&gpu, &frame)
        };
        let without = pixels(&mut view);
        view.select_look(Some(&look));
        assert!(view.profile_error.is_none(), "{:?}", view.profile_error);
        let chosen = view.camera_profile().look.expect("look recorded");
        assert_eq!(chosen.name, "Vivid");
        assert_eq!(chosen.content_hash.len(), 64);
        assert_ne!(
            without,
            pixels(&mut view),
            "the look should change the render"
        );

        // Switching the base DCP keeps the look.
        view.select_camera_profile(Some(&base));
        assert!(view.camera_profile().look.is_some());

        // Reloading the stored document restores the look.
        let doc = view.document().clone();
        view.replace_document(doc);
        assert!(view.profile_error.is_none(), "{:?}", view.profile_error);
        assert!(view.active_look.is_some());

        // A look changed on disk is reported, not silently re-rendered.
        std::fs::write(
            &look_path,
            nicti_calico::xmp_profile::testing::synthetic_valid_xmp("Vivid")
                .replace("Vivid", "Vivid2"),
        )
        .unwrap();
        let doc = view.document().clone();
        view.replace_document(doc);
        assert!(view.profile_error.is_some());
        assert!(view.active_look.is_none());

        view.select_look(None);
        assert!(view.camera_profile().look.is_none());
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

    /// Regression test (#49): `load_real_frame` used to record the photo identity with
    /// `set_own_hash(DECODE, ..)`, which the next render's `apply_document` silently overwrote
    /// with the stage default -- so two different photos at the same extent shared every baked
    /// cache key and the second showed the first one's pixels. Keys and pixels must differ.
    #[test]
    fn same_extent_photos_get_distinct_decode_keys_and_pixels() {
        use nicti_tapetum::frame::read_frame;
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));

        let first = Arc::new(synthetic_linear_frame());
        let mut other = synthetic_linear_frame();
        for px in &mut other.pixels {
            *px = 4095 - *px;
        }
        let second = Arc::new(other);
        assert_eq!((first.width, first.height), (second.width, second.height));

        view.load_real_frame(
            Arc::clone(&first),
            blake3::hash(b"photo one"),
            EditDocument::default(),
        );
        let pixels_one = read_frame(&gpu, &view.render());
        let key_one = view.graph.cache_key(DECODE).unwrap();

        view.load_real_frame(
            Arc::clone(&second),
            blake3::hash(b"photo two"),
            EditDocument::default(),
        );
        let pixels_two = read_frame(&gpu, &view.render());
        let key_two = view.graph.cache_key(DECODE).unwrap();

        assert_ne!(
            key_one, key_two,
            "each photo needs its own DECODE cache key"
        );
        assert_ne!(
            pixels_one, pixels_two,
            "photo two must not show photo one's bake"
        );

        // And the before/after view is the same photo, so it keeps the key.
        view.show_before = true;
        view.render();
        assert_eq!(view.graph.cache_key(DECODE).unwrap(), key_two);
    }

    // --- Global presence and effects (#380) through the real DevelopView ------------------------

    /// A global clarity/dehaze with no mask at all is rendered (the preview bakes first and builds
    /// the bases), and removing it returns the exact original pixels.
    #[test]
    fn a_global_presence_edit_changes_the_preview_with_no_masks() {
        use nicti_tapetum::coat::PresenceParams;
        use nicti_tapetum::frame::read_frame;
        use nicti_tapetum::stages::PRESENCE;
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        view.load_real_frame(
            Arc::new(synthetic_linear_frame()),
            blake3::hash(b"presence"),
            EditDocument::default(),
        );
        let base = read_frame(&gpu, &view.render());

        view.set_stage_params(
            PRESENCE,
            &PresenceParams {
                clarity: 0.9,
                dehaze: 0.6,
                ..Default::default()
            },
        );
        let edited = read_frame(&gpu, &view.render());
        let moved = base
            .iter()
            .zip(&edited)
            .filter(|(a, b)| (a[0] - b[0]).abs() + (a[1] - b[1]).abs() > 1e-3)
            .count();
        assert!(moved > base.len() / 10, "only {moved} pixels changed");

        view.reset_stage(PRESENCE);
        assert_eq!(read_frame(&gpu, &view.render()), base);
    }

    /// An Effects edit re-runs only the geometry pass (ADR-0044: never a bake, never the live
    /// suffix), darkens the corners and leaves the centre alone.
    #[test]
    fn an_effects_edit_only_reruns_the_geometry_pass() {
        use nicti_tapetum::coat::EffectsParams;
        use nicti_tapetum::frame::read_frame;
        use nicti_tapetum::stages::EFFECTS;
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        view.load_real_frame(
            Arc::new(synthetic_linear_frame()),
            blake3::hash(b"effects"),
            EditDocument::default(),
        );
        let base = read_frame(&gpu, &view.render());
        let (w, h) = (view.extent.width as usize, view.extent.height as usize);

        view.set_stage_params(
            EFFECTS,
            &EffectsParams {
                vignette_amount: -1.0,
                ..Default::default()
            },
        );
        let edited = read_frame(&gpu, &view.render());
        let stats = view.engine.renderer.last_stats();
        assert_eq!(stats.live_dispatches, 0, "{stats:?}");
        assert_eq!(stats.geometry_dispatches, 1, "{stats:?}");
        let (corner, centre) = (w * h - 1, (h / 2) * w + w / 2);
        assert!(edited[corner][0] < base[corner][0] * 0.5);
        assert_eq!(edited[centre], base[centre]);

        view.reset_stage(EFFECTS);
        assert_eq!(read_frame(&gpu, &view.render()), base);
    }

    // --- Local corrections (#49) through the real DevelopView -----------------------------------

    use nicti_tapetum::coat::MaskRecipe;
    use nicti_tapetum::mask::params::{
        LocalAdjust, LocalCorrection, MaskComponent, MaskGroup, MaskSource,
    };

    /// A +`stops` exposure correction over the left half of the frame (a hard-ish linear ramp).
    fn left_half(stops: f32) -> LocalCorrection {
        LocalCorrection {
            id: "left".into(),
            mask: MaskGroup {
                components: vec![MaskComponent {
                    source: MaskSource::LinearGradient {
                        p0: [0.45, 0.5],
                        p1: [0.55, 0.5],
                    },
                    ..MaskComponent::default()
                }],
            },
            adjust: LocalAdjust {
                exposure: stops,
                ..LocalAdjust::default()
            },
            ..LocalCorrection::default()
        }
    }

    fn subject_correction() -> LocalCorrection {
        LocalCorrection {
            id: "subject".into(),
            mask: MaskGroup {
                components: vec![MaskComponent {
                    source: MaskSource::Ai(MaskRecipe {
                        model_id: "test.model".into(),
                        model_version: "1".into(),
                        params: serde_json::json!({ "target": "subject" }),
                        seed: None,
                    }),
                    ..MaskComponent::default()
                }],
            },
            adjust: LocalAdjust {
                exposure: 1.0,
                ..LocalAdjust::default()
            },
            ..LocalCorrection::default()
        }
    }

    fn pixels(gpu: &Arc<GpuContext>, view: &mut DevelopView) -> Vec<[f32; 4]> {
        let f = view.render();
        nicti_tapetum::frame::read_frame(gpu, &f)
    }

    /// A NaN serializes to JSON `null`, which the canonical stage hasher refuses; it would reach
    /// `hash_value` through `apply_document` on the next render. The setter must scrub it, the same
    /// way a document loaded from disk is scrubbed.
    #[test]
    fn writing_finite_mask_params_never_truncates_them() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(gpu);
        // Two corrections whose brush points together exceed the document-wide budget: a write
        // must keep them all (the UI is still appending to the second stroke).
        let brush = |n: usize| {
            let mut c = left_half(1.0);
            c.mask.components[0].source = nicti_tapetum::mask::params::MaskSource::Brush {
                strokes: vec![nicti_tapetum::mask::params::Stroke {
                    points: vec![[0.5, 0.5]; n],
                    ..Default::default()
                }],
            };
            c
        };
        let n = nicti_tapetum::mask::params::MAX_BRUSH_POINTS;
        view.set_stage_params(
            MASKS,
            &MaskParams {
                corrections: vec![brush(n - 10), brush(100)],
            },
        );
        let stored: MaskParams = view.stage_params(MASKS);
        let len = |i: usize| match &stored.corrections[i].mask.components[0].source {
            nicti_tapetum::mask::params::MaskSource::Brush { strokes } => strokes[0].points.len(),
            _ => 0,
        };
        assert_eq!((len(0), len(1)), (n - 10, 100));
    }

    #[test]
    fn a_nan_written_into_the_mask_params_cannot_reach_the_graph_hash() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        let mut c = left_half(1.0);
        c.adjust.exposure = f32::NAN;
        c.amount = f32::INFINITY;
        view.set_stage_params(
            MASKS,
            &MaskParams {
                corrections: vec![c],
            },
        );
        let stored: MaskParams = view.stage_params(MASKS);
        let c = &stored.corrections[0];
        assert!(c.adjust.exposure.is_finite() && c.amount.is_finite());
        // Would panic hashing a `null` before the fix.
        let _ = pixels(&gpu, &mut view);
    }

    #[test]
    fn a_local_exposure_brightens_inside_the_mask_only_and_removing_it_restores_the_image() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        let base = pixels(&gpu, &mut view);

        view.set_stage_params(
            MASKS,
            &MaskParams {
                corrections: vec![left_half(1.0)],
            },
        );
        let masked = pixels(&gpu, &mut view);
        let w = 64usize;
        // Left of the ramp (fully selected): brighter. Right of it: untouched.
        let left = 8 * w + 6;
        let right = 8 * w + 58;
        assert!(
            masked[left][1] > base[left][1] * 1.3,
            "left: {:?} -> {:?}",
            base[left],
            masked[left]
        );
        assert_eq!(
            masked[right], base[right],
            "outside the mask nothing changes"
        );

        // The before/after toggle shows the unedited image, masks included.
        view.show_before = true;
        assert_eq!(pixels(&gpu, &mut view), base);
        view.show_before = false;
        assert_eq!(pixels(&gpu, &mut view), masked);

        view.reset_stage(MASKS);
        assert_eq!(
            pixels(&gpu, &mut view),
            base,
            "removing the entry restores the image"
        );
    }

    #[test]
    fn a_mask_correction_with_no_active_adjustment_costs_the_pipeline_nothing() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        let base = pixels(&gpu, &mut view);
        let mut c = left_half(0.0); // a mask with no adjustment yet
        c.enabled = true;
        view.set_stage_params(
            MASKS,
            &MaskParams {
                corrections: vec![c],
            },
        );
        assert_eq!(pixels(&gpu, &mut view), base);
    }

    #[test]
    fn the_neutral_key_ignores_every_edit_including_heal_but_follows_the_photo() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        view.load_real_frame(
            view.frame_arc(),
            blake3::hash(b"photo one"),
            EditDocument::default(),
        );
        view.render();
        let key = view.neutral_key();

        // Global tone / white balance / crop / detail edits...
        let mut exposure: ExposureParams = view.stage_params(EXPOSURE);
        exposure.stops = 1.5;
        view.set_stage_params(EXPOSURE, &exposure);
        let mut tone: ToneParams = view.stage_params(TONE);
        tone.contrast = 0.4;
        view.set_stage_params(TONE, &tone);
        view.set_stage_params(
            WB,
            &WbParams {
                temp_k: Some(4200.0),
                tint: 20.0,
            },
        );
        view.set_stage_params(
            CROP,
            &CropParams {
                x: 0.1,
                y: 0.1,
                width: 0.5,
                height: 0.5,
                rotation_degrees: 3.0,
            },
        );
        // ...a local edit...
        view.set_stage_params(
            MASKS,
            &MaskParams {
                corrections: vec![left_half(1.0)],
            },
        );
        // ...and a HEAL edit (the neutral render is post-lens, *pre-heal*, so healing a spot must
        // never re-run a model).
        view.set_stage_params(
            HEAL,
            &HealParams {
                spots: vec![nicti_tapetum::coat::Spot {
                    kind: nicti_tapetum::coat::SpotKind::Heal,
                    center: (20.0, 20.0),
                    radius: 5.0,
                    source_offset: Some((10.0, 0.0)),
                    feather: 2.0,
                    opacity: 1.0,
                    mask_recipe: None,
                }],
            },
        );
        view.render();
        assert_eq!(
            view.neutral_key(),
            key,
            "no edit may change what a model sees"
        );

        // A different photo does.
        view.load_real_frame(
            view.frame_arc(),
            blake3::hash(b"photo two"),
            EditDocument::default(),
        );
        view.render();
        assert_ne!(view.neutral_key(), key);
    }

    #[test]
    fn an_ai_mask_needs_a_bake_shows_nothing_until_it_arrives_then_appears() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        view.load_real_frame(
            view.frame_arc(),
            blake3::hash(b"photo"),
            EditDocument::default(),
        );
        let base = pixels(&gpu, &mut view);
        view.set_stage_params(
            MASKS,
            &MaskParams {
                corrections: vec![subject_correction()],
            },
        );
        // Not baked yet: it is requested, and the mask selects nothing.
        let reqs = view.mask_bake_requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            pixels(&gpu, &mut view),
            base,
            "an unbaked mask must select nothing"
        );

        // The model finishes: a left-half subject at model resolution.
        let alpha = AiAlpha::new(
            16,
            16,
            (0..256)
                .map(|i| if i % 16 < 8 { 1.0 } else { 0.0 })
                .collect(),
        )
        .unwrap();
        view.set_ai_alpha(reqs[0].key, Arc::new(alpha));
        assert!(view.has_ai_alpha(&reqs[0].key));
        assert!(view.mask_bake_requests().is_empty(), "nothing left to bake");
        let with = pixels(&gpu, &mut view);
        let w = 64usize;
        assert!(
            with[8 * w + 4][1] > base[8 * w + 4][1] * 1.3,
            "the subject half is brightened"
        );
        assert_eq!(
            with[8 * w + 60],
            base[8 * w + 60],
            "the background is untouched"
        );
    }

    /// The bug the DECODE-identity fix exists for, tested at the level that matters: a mask made
    /// on one photo must never show up on another.
    #[test]
    fn an_ai_mask_from_one_photo_never_leaks_onto_the_next() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        view.load_real_frame(
            view.frame_arc(),
            blake3::hash(b"photo A"),
            EditDocument::default(),
        );
        view.set_stage_params(
            MASKS,
            &MaskParams {
                corrections: vec![subject_correction()],
            },
        );
        let key_a = view.mask_bake_requests()[0].key;
        view.set_ai_alpha(key_a, Arc::new(AiAlpha::new(4, 4, vec![1.0; 16]).unwrap()));
        view.render();
        assert!(view.mask_bake_requests().is_empty());

        // Photo B: same extent, and the same masks pasted onto it (loading a photo starts a fresh
        // edit document, so re-apply them -- as a synced/pasted edit would).
        view.load_real_frame(
            view.frame_arc(),
            blake3::hash(b"photo B"),
            EditDocument::default(),
        );
        assert!(
            !view.has_ai_alpha(&key_a),
            "the finished alpha was released"
        );
        view.set_stage_params(
            MASKS,
            &MaskParams {
                corrections: vec![subject_correction()],
            },
        );
        let reqs = view.mask_bake_requests();
        assert_eq!(reqs.len(), 1, "photo B needs its own bake");
        assert_ne!(
            reqs[0].key, key_a,
            "and its bake key differs from photo A's"
        );
    }

    /// #52: a batch paste/sync/undo rewrites the loaded photo in the catalog, so the view must take
    /// the new document as already saved (else the autosave writes its old copy back over the
    /// batch), drop alphas the document no longer references, and leave a pasted AI mask to
    /// re-bake lazily.
    #[test]
    fn replace_document_is_saved_and_leaves_ai_masks_to_rebake() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        view.load_real_frame(
            view.frame_arc(),
            blake3::hash(b"photo"),
            EditDocument::default(),
        );
        view.set_stage_params(
            MASKS,
            &MaskParams {
                corrections: vec![subject_correction()],
            },
        );
        let with_mask = view.document().clone();
        let key = view.mask_bake_requests()[0].key;
        view.set_ai_alpha(key, Arc::new(AiAlpha::new(4, 4, vec![1.0; 16]).unwrap()));
        assert!(view.is_dirty());

        view.replace_document(EditDocument::default());
        assert!(!view.is_dirty(), "the batch already wrote it");
        assert_eq!(view.document(), &EditDocument::default());
        assert!(!view.has_ai_alpha(&key), "no mask references it any more");

        view.replace_document(with_mask.clone());
        assert!(!view.is_dirty());
        assert_eq!(view.document(), &with_mask);
        assert_eq!(
            view.mask_bake_requests().len(),
            1,
            "a pasted AI mask bakes on this photo when it's next shown"
        );
    }

    #[test]
    fn pruning_drops_alphas_of_deleted_masks_but_keeps_a_disabled_ones() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        let mut c = subject_correction();
        view.set_stage_params(
            MASKS,
            &MaskParams {
                corrections: vec![c.clone()],
            },
        );
        let key = view.mask_bake_requests()[0].key;
        view.set_ai_alpha(key, Arc::new(AiAlpha::new(2, 2, vec![1.0; 4]).unwrap()));

        c.enabled = false; // toggled off: the alpha is worth keeping
        view.set_stage_params(
            MASKS,
            &MaskParams {
                corrections: vec![c],
            },
        );
        view.prune_ai_alphas();
        assert!(view.has_ai_alpha(&key));

        view.reset_stage(MASKS); // deleted: released
        view.prune_ai_alphas();
        assert!(!view.has_ai_alpha(&key));
    }

    /// ADR-0101: an Auto click whose result equals what's already there must not touch the
    /// document (no dirtying, no history step).
    #[test]
    fn a_second_auto_tone_click_is_unchanged_and_leaves_the_document_alone() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        let first = view.apply_auto_tone();
        assert!(
            matches!(
                first,
                AutoApplied::Applied | AutoApplied::AppliedLowConfidence(_)
            ),
            "{first:?}"
        );

        let before = view.document().clone();
        assert_eq!(view.apply_auto_tone(), AutoApplied::Unchanged);
        assert_eq!(view.document(), &before);
    }

    /// A uniform frame has no edges at all: auto-level must report why it did nothing, and must
    /// not even create a crop entry.
    #[test]
    fn auto_straighten_on_a_featureless_frame_is_skipped_and_writes_nothing() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        let mut flat = synthetic_linear_frame();
        flat.pixels.fill(2000);
        view.load_real_frame(
            Arc::new(flat),
            blake3::hash(b"flat"),
            EditDocument::default(),
        );
        assert_eq!(
            view.apply_auto_straighten(),
            AutoApplied::Skipped {
                reason: AutoReason::NoFeatures,
                low_confidence: false,
            }
        );
        assert!(!view.document().stages.contains_key(CROP));
    }

    #[test]
    fn auto_ops_refuse_an_incomplete_frame() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        let mut truncated = synthetic_linear_frame();
        truncated.pixels.truncate(10);
        view.frame = Arc::new(truncated);
        let skipped = AutoApplied::Skipped {
            reason: AutoReason::DecodeIncomplete,
            low_confidence: false,
        };
        assert_eq!(view.apply_auto_tone(), skipped);
        assert_eq!(view.apply_auto_straighten(), skipped);
        assert!(!view.has_edits());
    }

    /// #272: the idle preview is sized to the crop rect; the crop/heal/mask tools and Before show
    /// the whole frame.
    #[test]
    fn the_idle_preview_is_the_crop_rect_and_a_tool_shows_the_whole_frame() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        let (w, h) = view.source_extent();
        let (cw, ch) = ((w / 2.0).floor(), (h / 4.0).floor());
        view.set_stage_params(
            CROP,
            &CropParams {
                x: 2.0,
                y: 3.0,
                width: cw,
                height: ch,
                rotation_degrees: 0.0,
            },
        );
        let out = view.render();
        assert_eq!(
            (out.extent.width, out.extent.height),
            (cw as u32, ch as u32)
        );
        assert_eq!(view.display_extent(), (cw, ch));

        view.uncropped_preview = true;
        let full = view.render();
        assert_eq!(
            (full.extent.width as f32, full.extent.height as f32),
            (w, h)
        );
        view.uncropped_preview = false;

        view.show_before = true;
        let before = view.render();
        assert_eq!(
            (before.extent.width as f32, before.extent.height as f32),
            (w, h)
        );
    }

    /// #272: with no rotation, the cropped canvas's pixel (0, 0) is the uncropped frame's pixel at
    /// the crop origin -- so the crop is the same picture, just resized.
    #[test]
    fn the_cropped_canvas_matches_the_uncropped_frame_at_the_crop_origin() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut view = DevelopView::new(Arc::clone(&gpu));
        let (w, h) = view.source_extent();
        let (x, y) = (4u32, 6u32);
        view.set_stage_params(
            CROP,
            &CropParams {
                x: x as f32,
                y: y as f32,
                width: w / 2.0,
                height: h / 2.0,
                rotation_degrees: 0.0,
            },
        );
        let cropped = nicti_tapetum::frame::read_frame(&gpu, &view.render());
        view.uncropped_preview = true;
        let full_frame = view.render();
        let full = nicti_tapetum::frame::read_frame(&gpu, &full_frame);
        // Compare the interior pixel (1, 1) to stay off the bilinear edge.
        let c = cropped[(w / 2.0) as usize + 1];
        let f = full[((y + 1) * full_frame.extent.width + x + 1) as usize];
        for i in 0..3 {
            assert!((c[i] - f[i]).abs() < 1e-2, "{c:?} vs {f:?}");
        }
    }

    /// #272: `display_to_source_norm` is the inverse of what the crop kernel samples.
    #[test]
    fn display_to_source_norm_follows_the_crop_and_its_rotation() {
        let mut doc = DevelopDoc::new();
        let (w, h) = doc.source_extent();
        // No crop: identity.
        let n = doc.display_to_source_norm([0.25, 0.75]);
        assert!((n[0] - 0.25).abs() < 1e-5 && (n[1] - 0.75).abs() < 1e-5);
        // Unrotated crop: the display origin is the crop origin, the far corner is its far corner.
        doc.set_stage_params(
            CROP,
            &CropParams {
                x: w / 4.0,
                y: h / 4.0,
                width: w / 2.0,
                height: h / 2.0,
                rotation_degrees: 0.0,
            },
        );
        let o = doc.display_to_source_norm([0.0, 0.0]);
        assert!((o[0] - 0.25).abs() < 0.02 && (o[1] - 0.25).abs() < 0.02);
        let far = doc.display_to_source_norm([1.0, 1.0]);
        assert!((far[0] - 0.75).abs() < 0.02 && (far[1] - 0.75).abs() < 0.02);
        // Rotation keeps the display centre on the crop centre.
        let mut crop: CropParams = doc.stage_params(CROP);
        crop.rotation_degrees = 12.0;
        doc.set_stage_params(CROP, &crop);
        let c = doc.display_to_source_norm([0.5, 0.5]);
        assert!((c[0] - 0.5).abs() < 0.02 && (c[1] - 0.5).abs() < 0.02);
        // Off-centre with rotation: must equal the crop kernel's own affine (not just the centre,
        // which any rotation about the crop centre leaves fixed).
        let rect = crop.effective_rect((w, h));
        let (dw, dh) = doc.display_extent();
        let (ex, ey) = geometry::affine_for_crop(rect, 12.0).apply((0.9 * dw, 0.2 * dh));
        let got = doc.display_to_source_norm([0.9, 0.2]);
        assert!((got[0] - ex / w).abs() < 1e-5 && (got[1] - ey / h).abs() < 1e-5);
        assert!(
            (got[0] - 0.75).abs() > 1e-3 || (got[1] - 0.25).abs() > 1e-3,
            "a 12 degree rotation must move an off-centre point"
        );
        // A tool showing the whole frame is the identity again.
        doc.uncropped_preview = true;
        assert_eq!(doc.display_to_source_norm([0.1, 0.9]), [0.1, 0.9]);
    }

    /// A drag of one slider is many `set_stage_params` calls but one undo step, and its undo
    /// target is the pre-drag value (#324).
    #[test]
    fn a_slider_drag_is_one_undo_step_back_to_the_pre_drag_value() {
        let mut doc = DevelopDoc::new();
        for stops in [0.2, 0.5, 0.9, 1.3] {
            doc.set_stage_params(EXPOSURE, &ExposureParams { stops });
        }
        assert!(doc.can_undo() && !doc.can_redo());
        assert!(doc.undo());
        assert_eq!(doc.stage_params::<ExposureParams>(EXPOSURE).stops, 0.0);
        assert!(!doc.can_undo(), "the whole drag was a single step");
        assert!(doc.redo());
        assert_eq!(doc.stage_params::<ExposureParams>(EXPOSURE).stops, 1.3);
        assert!(!doc.redo());
    }

    /// ADR-0101 rule 6 at the Develop level: the panels write every stage every frame, so an
    /// unchanged value (including a default over an absent entry) must create no step and must
    /// not dirty the document.
    #[test]
    fn rewriting_the_effective_value_makes_no_step_and_no_dirt() {
        let mut doc = DevelopDoc::new();
        doc.set_stage_params(EXPOSURE, &ExposureParams::default());
        doc.set_stage_params(TONE, &ToneParams::default());
        assert!(!doc.can_undo() && !doc.is_dirty() && !doc.has_edits());

        doc.set_stage_params(EXPOSURE, &ExposureParams { stops: 1.0 });
        assert!(doc.undo());
        assert!(doc.can_redo());
        // A no-op write must not truncate the redo stack.
        doc.set_stage_params(EXPOSURE, &ExposureParams::default());
        assert!(doc.can_redo(), "an unchanged write keeps redo");
    }

    /// Undoing back to what the catalog holds is not dirty, so the autosave has nothing to write.
    #[test]
    fn undoing_back_to_the_saved_document_is_clean() {
        let mut doc = DevelopDoc::new();
        doc.set_stage_params(EXPOSURE, &ExposureParams { stops: 1.0 });
        assert!(doc.is_dirty());
        doc.undo();
        assert!(!doc.is_dirty());
        doc.redo();
        assert!(doc.is_dirty());
    }

    #[test]
    fn reset_stage_is_an_undoable_step() {
        let mut doc = DevelopDoc::new();
        doc.set_stage_params(EXPOSURE, &ExposureParams { stops: 1.0 });
        doc.mark_saved();
        // Not within the drag window of the write above: a separate gesture.
        std::thread::sleep(std::time::Duration::from_millis(
            GESTURE_WINDOW_MS as u64 + 50,
        ));
        doc.reset_stage(EXPOSURE);
        assert!(!doc.has_edits());
        assert!(doc.undo());
        assert_eq!(doc.stage_params::<ExposureParams>(EXPOSURE).stops, 1.0);
        doc.reset_stage(CROP);
        assert!(
            doc.can_redo(),
            "resetting an absent stage is no step and keeps redo"
        );
    }

    /// Exposure + Tone written together (Auto tone) undo together.
    #[test]
    fn a_multi_stage_edit_is_one_step() {
        let mut doc = DevelopDoc::new();
        let exposure = ExposureParams { stops: 0.7 };
        let tone = ToneParams {
            contrast: 20.0,
            ..ToneParams::default()
        };
        let edits = [
            doc.stage_edit(EXPOSURE, &exposure),
            doc.stage_edit(TONE, &tone),
            doc.stage_edit(WB, &WbParams::default()),
        ];
        doc.apply_stage_edits(edits.into_iter().flatten().collect());
        assert_eq!(
            doc.document().stages.len(),
            2,
            "the unchanged WB is skipped"
        );
        assert!(doc.undo());
        assert!(!doc.has_edits());
        assert!(!doc.can_undo());
    }

    /// Loading a photo (or a pasted document) starts a fresh history: Undo never reaches into the
    /// previous photo's edits.
    #[test]
    fn loading_a_document_clears_the_history() {
        let mut doc = DevelopDoc::new();
        doc.set_stage_params(EXPOSURE, &ExposureParams { stops: 1.0 });
        let mut other = EditDocument::default();
        other.stages.insert(
            TONE.to_string(),
            StageEntry {
                schema_version: 1,
                params: serde_json::to_value(ToneParams {
                    contrast: 5.0,
                    ..ToneParams::default()
                })
                .unwrap(),
            },
        );
        doc.replace_document(other.clone());
        assert!(!doc.can_undo() && !doc.undo());
        assert_eq!(doc.document(), &other);
        doc.set_stage_params(EXPOSURE, &ExposureParams { stops: 2.0 });
        assert!(doc.undo());
        assert_eq!(doc.document(), &other, "undo stops at the loaded document");
    }
}
