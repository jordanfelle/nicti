//! The one production render-graph shape and the "edit document -> kernel inputs" step, shared by
//! everything that renders a photo: the interactive Develop view (`nicti-pelt::render`), the export
//! pipeline (#57, `nicti-pelt::export`) and the real-NEF harness (`bench/knead`). Lifted out of
//! `nicti-pelt/src/render.rs` (#57) so those callers can't drift apart -- a Develop preview and an
//! export of the same document must resolve the same params, and before this module each had (or
//! would have needed) its own copy of the graph, the registry and the params resolution.
//!
//! `build_graph`'s node ids and `build_registry`'s entries must stay in sync:
//! `RenderGraph::apply_document` errors on any graph node id missing from the registry.

use std::sync::Arc;

use nicti_calico::dcp::DcpProfile;
use nicti_calico::profile::ProfileSolution;
use nicti_cornea::LinearFrame;
use nicti_pawprint::EditDocument;

use crate::coat::{
    self, CameraProfileParams, ColorGradeParams, CropParams, DefringeParams, EffectsParams,
    ExposureParams, HealParams, HslParams, LensParams, NoiseReductionParams, PointColorParams,
    PointCurveParams, PresenceParams, SharpenParams, ToneCurveParams, ToneParams, VibranceParams,
    WbParams,
};
use crate::color;
use crate::frame::Extent;
use crate::geometry::{self, Affine2D, CropRect};
use crate::graph::{RenderGraph, StageKind, StageNode};
use crate::stages::{
    self, LiveParams, COLOR_GRADE, CROP, DECODE, DEFRINGE, DEMOSAIC, DENOISE, EFFECTS, EXPOSURE,
    HEAL, HSL, LENS, MASKS, NEUTRAL, NOISE_REDUCTION, POINT_COLOR, POINT_CURVE, PRESENCE, SHARPEN,
    TONE, TONE_CURVE, VIBRANCE, WB, WORKING_SPACE,
};
use crate::{RenderStage, StageRegistry};

/// The baked prefix, in dependency order.
pub const BAKED_IDS: [&str; 5] = [DECODE, DEMOSAIC, DENOISE, LENS, HEAL];

/// The fused live suffix, in dependency order.
pub const LIVE_IDS: [&str; 15] = [
    WB,
    WORKING_SPACE,
    // Purple/green fringe desaturation (#428): same dispatch, right after the camera->working matrix.
    DEFRINGE,
    EXPOSURE,
    TONE,
    TONE_CURVE,
    // Freeform RGB/R/G/B point curves (#432), right after the parametric curve.
    POINT_CURVE,
    VIBRANCE,
    PRESENCE,
    HSL,
    // Color Grading then Point Color (#432), both OkLab, right after HSL.
    COLOR_GRADE,
    POINT_COLOR,
    SHARPEN,
    NOISE_REDUCTION,
    // Local corrections (#49): applied inside the same fused live dispatch as everything above.
    MASKS,
];

/// The geometry nodes, one fused sample pass: the crop and the post-crop effects (#380).
pub const GEOMETRY_IDS: [&str; 2] = [CROP, EFFECTS];

/// The full graph: baked prefix -> live suffix -> crop -> effects.
pub fn build_graph() -> RenderGraph {
    let mut graph = RenderGraph::new();
    let mut prev: Option<&str> = None;
    for id in BAKED_IDS {
        graph
            .add_node(StageNode {
                id: id.to_string(),
                kind: StageKind::Baked,
                upstream: prev.map(|p| vec![p.to_string()]).unwrap_or_default(),
                own_hash: blake3::hash(id.as_bytes()),
            })
            .expect("static graph ids are unique");
        prev = Some(id);
    }
    // The neutral render AI masks infer on (#49, ADR-0049): post-lens, *pre-heal*, so a heal edit
    // never re-runs a model. A keying-only node -- nothing renders it (it is not in the baked
    // chain), it exists so an AI bake key chains from LENS and can never depend on a slider.
    graph
        .add_node(StageNode {
            id: NEUTRAL.to_string(),
            kind: StageKind::Baked,
            upstream: vec![LENS.to_string()],
            own_hash: blake3::hash(NEUTRAL.as_bytes()),
        })
        .expect("static graph ids are unique");
    for id in LIVE_IDS {
        graph
            .add_node(StageNode {
                id: id.to_string(),
                kind: StageKind::Live,
                upstream: vec![prev.expect("baked prefix is non-empty").to_string()],
                own_hash: blake3::hash(id.as_bytes()),
            })
            .expect("static graph ids are unique");
        prev = Some(id);
    }
    graph
        .add_node(StageNode {
            id: CROP.to_string(),
            kind: StageKind::Geometry,
            upstream: vec![prev.expect("live suffix is non-empty").to_string()],
            own_hash: blake3::hash(CROP.as_bytes()),
        })
        .expect("static graph ids are unique");
    graph
        .add_node(StageNode {
            id: EFFECTS.to_string(),
            kind: StageKind::Geometry,
            upstream: vec![CROP.to_string()],
            own_hash: blake3::hash(EFFECTS.as_bytes()),
        })
        .expect("static graph ids are unique");
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
render_stage_factory!(point_curve_factory, stages::point_curve_stage);
render_stage_factory!(vibrance_factory, stages::vibrance_stage);
render_stage_factory!(presence_factory, stages::presence_stage);
render_stage_factory!(defringe_factory, stages::defringe_stage);
render_stage_factory!(hsl_factory, stages::hsl_stage);
render_stage_factory!(color_grade_factory, stages::color_grade_stage);
render_stage_factory!(point_color_factory, stages::point_color_stage);
render_stage_factory!(sharpen_factory, stages::sharpen_stage);
render_stage_factory!(noise_reduction_factory, stages::noise_reduction_stage);
render_stage_factory!(crop_factory, stages::crop_stage);
render_stage_factory!(effects_factory, stages::effects_stage);
render_stage_factory!(neutral_factory, stages::neutral_stage);
render_stage_factory!(masks_factory, stages::masks_stage);

type StageFactoryEntry = (&'static str, fn() -> Arc<dyn RenderStage>);

/// Every stage [`build_graph`] can reference.
pub fn build_registry() -> StageRegistry {
    let mut registry = StageRegistry::new();
    let entries: [StageFactoryEntry; 23] = [
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
        (POINT_CURVE, point_curve_factory),
        (VIBRANCE, vibrance_factory),
        (PRESENCE, presence_factory),
        (DEFRINGE, defringe_factory),
        (HSL, hsl_factory),
        (COLOR_GRADE, color_grade_factory),
        (POINT_COLOR, point_color_factory),
        (SHARPEN, sharpen_factory),
        (NOISE_REDUCTION, noise_reduction_factory),
        (CROP, crop_factory),
        (EFFECTS, effects_factory),
        (NEUTRAL, neutral_factory),
        (MASKS, masks_factory),
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

/// Stamps the loaded photo's identity into `doc`'s `DECODE` entry.
///
/// Call this on the **render-time copy** of a document (never the one stored in the catalog),
/// every render. `RenderGraph::apply_document` recomputes every node's `own_hash` from the
/// document, so identity set directly with `set_own_hash(DECODE, ..)` is reset to the stage default
/// on the next render -- and two different photos of the same pixel size would then share baked
/// cache keys and serve each other's pixels. Living in the document, the identity is reapplied
/// identically each time (no per-frame invalidation) and `DecodeExec` ignores the params.
pub fn stamp_source_identity(doc: &mut EditDocument, identity: blake3::Hash) {
    doc.stages.insert(
        DECODE.to_string(),
        nicti_pawprint::StageEntry {
            schema_version: 1,
            params: serde_json::json!({ "source": identity.to_hex().to_string() }),
        },
    );
}

/// Cache key of the neutral render AI masks infer on (the keying-only `NEUTRAL` node) for a photo
/// with identity `identity` and edit document `doc` -- what `ai_bake_key` chains from.
///
/// Needs no pixels and no GPU, so a caller that is *not* looking at the photo (the background
/// pre-bake, #353) can name the bakes its masks need. It is the same key `DevelopView::neutral_key`
/// reports once that photo is loaded and `doc` applied (a test in `nicti-pelt` pins this): `NEUTRAL`
/// sits upstream of heal, the masks and every live stage, so only the identity and the baked
/// prefix's own entries reach it.
pub fn neutral_key(doc: &EditDocument, identity: blake3::Hash) -> blake3::Hash {
    let mut graph = build_graph();
    let mut doc = doc.clone();
    stamp_source_identity(&mut doc, identity);
    graph
        .apply_document(&doc, &build_registry())
        .expect("build_registry covers every id build_graph adds");
    graph
        .cache_key(NEUTRAL)
        .expect("build_graph always adds NEUTRAL")
}

/// One stage's typed params out of a document (`T::default()` when the document has no entry).
pub fn resolve<T: serde::de::DeserializeOwned + Default>(doc: &EditDocument, id: &str) -> T {
    match doc.stages.get(id) {
        Some(entry) => coat::parse(&entry.params),
        None => T::default(),
    }
}

/// Everything a render needs beyond the graph itself, resolved from one document.
pub struct RenderInputs {
    /// For `LiveSuffixKernel::set_params`.
    pub live: LiveParams,
    /// For `LensExec::params` (#428).
    pub lens: LensParams,
    /// For `HealExec::params`.
    pub heal: HealParams,
    /// The crop rect in source pixels (the full frame for a default crop).
    pub crop_rect: CropRect,
    /// Rotation composed with the crop offset; for `CropKernel::set_transform` and, at export,
    /// `TiledRender::new`'s base transform.
    pub crop_transform: Affine2D,
    /// Post-crop vignette and grain (#380), sanitized.
    pub effects: EffectsParams,
}

impl RenderInputs {
    /// Binds the post-crop effects to the geometry pass. `crop_transform` here is the *untiled,
    /// unscaled* crop transform on purpose: the effects are evaluated in crop-normalized
    /// coordinates, so a screen-size preview or one export tile (whose own output -> source
    /// transform is scaled or offset) sees the same pattern as the whole frame. Call it every
    /// render -- the kernel is reused across photos and a noop `effects` clears the previous one.
    pub fn bind_effects(&self, crop: &crate::stages::CropKernel) {
        crop.set_effects(
            &self.effects,
            self.crop_transform,
            self.crop_rect.width,
            self.crop_rect.height,
        );
    }
}
/// Resolves `doc` into kernel inputs for `frame` at `extent` -- the "document -> params" half of a
/// render, previously inlined in `DevelopView::render`.
///
/// `look` is the Look `.xmp` the document's `CameraProfileParams.look` names, loaded and
/// verified by the caller. `profile` is the DCP camera profile the caller has loaded and verified; it is only used when the
/// document's `WORKING_SPACE` entry actually selects one (`content_hash` set), so an empty document
/// (the before view) always gets the plain LibRaw matrix. `pixel_scale` is render long edge over
/// source long edge (`1.0` for a native-extent render).
pub fn resolve_inputs(
    doc: &EditDocument,
    frame: &LinearFrame,
    extent: Extent,
    profile: Option<&DcpProfile>,
    look: Option<&nicti_calico::xmp_profile::LookProfile>,
    pixel_scale: f32,
) -> RenderInputs {
    let wb: WbParams = resolve(doc, WB);
    let crop: CropParams = resolve(doc, CROP);
    let crop_rect = crop.effective_rect((extent.width as f32, extent.height as f32));
    let crop_transform = geometry::affine_for_crop(crop_rect, crop.rotation_degrees);

    // A selected DCP profile (#42) replaces the plain LibRaw matrix with its own
    // illuminant-interpolated matrix (WB folded in per the DNG ForwardMatrix contract) and adds the
    // HueSatMap / baseline exposure / LookTable stages.
    let chosen: CameraProfileParams = resolve(doc, WORKING_SPACE);
    let solution: Option<Arc<ProfileSolution>> = match (&chosen.content_hash, profile) {
        (Some(_), Some(profile)) => {
            let gains = color::wb_gains_with_params(frame.cam_mul, &frame.cam_xyz, &wb);
            let solved = profile.solve(gains.map(f64::from));
            // A Look only layers on a selected DCP, and only if the document names one.
            Some(Arc::new(match (&chosen.look, look) {
                (Some(_), Some(look)) => solved.with_look(look),
                _ => solved,
            }))
        }
        _ => None,
    };
    let working_space_matrix = match &solution {
        Some(s) => s.camera_to_working,
        None => color::camera_to_working_space_matrix(frame.cam_mul, &frame.cam_xyz, &wb),
    };

    let exposure: ExposureParams = resolve(doc, EXPOSURE);
    let tone: ToneParams = resolve(doc, TONE);
    let tone_curve: ToneCurveParams = resolve(doc, TONE_CURVE);
    let point_curve: PointCurveParams = resolve::<PointCurveParams>(doc, POINT_CURVE).sanitized();
    let vibrance: VibranceParams = resolve(doc, VIBRANCE);
    let presence: PresenceParams = resolve::<PresenceParams>(doc, PRESENCE).sanitized();
    let hsl: HslParams = resolve(doc, HSL);
    let color_grade: ColorGradeParams = resolve::<ColorGradeParams>(doc, COLOR_GRADE).sanitized();
    let point_color: PointColorParams = resolve::<PointColorParams>(doc, POINT_COLOR).sanitized();
    let sharpen: SharpenParams = resolve(doc, SHARPEN);
    let noise_reduction: NoiseReductionParams = resolve(doc, NOISE_REDUCTION);

    RenderInputs {
        live: LiveParams {
            working_space_matrix,
            camera_profile: solution,
            exposure,
            tone,
            tone_curve,
            point_curve,
            color_grade,
            point_color,
            vibrance,
            presence,
            defringe: resolve::<DefringeParams>(doc, DEFRINGE).sanitized(),
            hsl,
            sharpen,
            noise_reduction,
            pixel_scale,
        },
        lens: resolve(doc, LENS),
        heal: resolve(doc, HEAL),
        crop_rect,
        crop_transform,
        effects: resolve::<EffectsParams>(doc, EFFECTS).sanitized(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> blake3::Hash {
        blake3::hash(&[n])
    }

    #[test]
    fn the_neutral_key_follows_the_photo_and_not_the_edits_downstream_of_it() {
        let plain = neutral_key(&EditDocument::default(), id(1));
        assert_eq!(
            plain,
            neutral_key(&EditDocument::default(), id(1)),
            "stable"
        );
        assert_ne!(
            plain,
            neutral_key(&EditDocument::default(), id(2)),
            "per photo"
        );

        // A slider, a heal spot and the masks themselves are all downstream of NEUTRAL: none of
        // them may re-key an AI bake.
        let mut edited = EditDocument::default();
        for (stage, params) in [
            (EXPOSURE, serde_json::json!({ "stops": 1.5 })),
            (HEAL, serde_json::json!({ "spots": [] })),
            (MASKS, serde_json::json!({ "corrections": [] })),
            (
                PRESENCE,
                serde_json::json!({ "clarity": 0.5, "dehaze": 0.4 }),
            ),
            (
                EFFECTS,
                serde_json::json!({ "vignette_amount": -0.5, "grain_amount": 0.5 }),
            ),
        ] {
            edited.stages.insert(
                stage.to_string(),
                nicti_pawprint::StageEntry {
                    schema_version: 1,
                    params,
                },
            );
        }
        assert_eq!(plain, neutral_key(&edited, id(1)));
    }

    #[test]
    fn the_registry_covers_every_graph_node() {
        let mut graph = build_graph();
        // `apply_document` errors on any node id the registry doesn't know.
        graph
            .apply_document(&EditDocument::default(), &build_registry())
            .expect("registry covers the graph");
    }

    #[test]
    fn apply_document_overwrites_a_hash_set_directly_on_the_decode_node() {
        // Why photo identity goes into the *document* (stamp_source_identity) instead of
        // `set_own_hash(DECODE, ..)`: apply_document recomputes every node's hash from the
        // document, so a hash set directly is silently reset on the next render.
        let mut graph = build_graph();
        let registry = build_registry();
        graph
            .apply_document(&EditDocument::default(), &registry)
            .unwrap();
        let default_key = graph.cache_key(DECODE).unwrap();
        graph
            .set_own_hash(DECODE, blake3::hash(b"photo A"))
            .unwrap();
        assert_ne!(graph.cache_key(DECODE).unwrap(), default_key);
        graph
            .apply_document(&EditDocument::default(), &registry)
            .unwrap();
        assert_eq!(graph.cache_key(DECODE).unwrap(), default_key);
    }

    #[test]
    fn a_stamped_identity_survives_apply_document_and_separates_photos() {
        let registry = build_registry();
        let key_for = |graph: &mut RenderGraph, identity: &[u8]| {
            let mut doc = EditDocument::default();
            stamp_source_identity(&mut doc, blake3::hash(identity));
            graph.apply_document(&doc, &registry).unwrap();
            // Every downstream key chains from DECODE, so check the last baked node.
            graph.cache_key(HEAL).unwrap()
        };
        let mut graph = build_graph();
        let a = key_for(&mut graph, b"photo A");
        let b = key_for(&mut graph, b"photo B");
        assert_ne!(a, b, "two photos must never share cache keys");
        // Stable under repeated application (no per-frame invalidation), and back to A again.
        assert_eq!(key_for(&mut graph, b"photo B"), b);
        assert_eq!(key_for(&mut graph, b"photo A"), a);
        // Edits to a live stage don't touch the baked chain's keys.
        let mut doc = EditDocument::default();
        stamp_source_identity(&mut doc, blake3::hash(b"photo A"));
        doc.stages.insert(
            EXPOSURE.to_string(),
            nicti_pawprint::StageEntry {
                schema_version: 1,
                params: serde_json::json!({ "stops": 1.0 }),
            },
        );
        graph.apply_document(&doc, &registry).unwrap();
        assert_eq!(graph.cache_key(HEAL).unwrap(), a);
    }

    #[test]
    fn graph_has_baked_then_live_then_crop() {
        let graph = build_graph();
        for id in BAKED_IDS.iter().chain(LIVE_IDS.iter()).chain([&CROP]) {
            assert!(graph.node(id).is_some(), "missing node {id}");
        }
    }
}
