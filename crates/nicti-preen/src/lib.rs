//! Export engine (ADR-0019 §7/§8, ADR-0056, #57).
//!
//! [`Exporter`] is the extension point (identity/versioning via `Module`, plus the format-specific
//! encode step); [`export_frame`] is the one entry point that turns a rendered linear working-space
//! frame plus an [`spec::ExportSpec`] into a finished file's bytes. Everything else here is a
//! building block of it:
//!
//! | module | job |
//! | --- | --- |
//! | [`spec`] | the settings model, presets, validation |
//! | [`naming`] | filename/subfolder tokens, Windows-safe components |
//! | [`plan`] | per-photo output paths + collision handling, decided before rendering |
//! | [`write`] | collision-safe atomic file output |
//! | [`resize`] / [`orient`] / [`color`] / [`watermark`] | the format-independent pixel steps |
//! | [`metadata`] | source EXIF read; EXIF/XMP build + embed |
//! | [`exporters`] | the built-in JPEG/PNG/TIFF encoders and [`exporters::builtin_registry`] |
//!
//! This crate is GPU-free and catalog-free by design: it consumes a host-memory frame and plain
//! data ([`naming::AssetFacts`], [`metadata::SourceMetadata`]), so it depends on neither
//! `nicti-tapetum` nor `nicti-lair`. The render + job wiring lives in `nicti-pelt::export`.

pub mod color;
pub mod exporters;
pub mod metadata;
pub mod naming;
pub mod orient;
pub mod plan;
pub mod resize;
pub mod spec;
pub mod watermark;
pub mod write;

use nicti_calico::space::OutputSpace;
use nicti_claw::{Module, Registry};

use crate::color::OutputPixels;
use crate::metadata::{ExifContext, ExifPayload, SourceMetadata};
use crate::spec::{BitDepth, ExportFormat, ExportSpec, FormatSpec};
use crate::watermark::WatermarkSource;

/// Which metadata containers an exporter can write into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbedSupport {
    pub exif: bool,
    pub xmp: bool,
    pub icc: bool,
    pub dpi: bool,
}

/// Quantized pixels ready to encode.
pub struct OutputImage<'a> {
    pub width: u32,
    pub height: u32,
    pub pixels: &'a OutputPixels,
}

/// What to attach to the encoded file. An exporter ignores what its container can't hold
/// ([`Exporter::embeds`]).
pub struct Embed<'a> {
    /// ICC profile bytes; empty = none.
    pub icc: &'a [u8],
    pub dpi: u32,
    pub exif: Option<&'a ExifPayload>,
    /// A complete XMP packet.
    pub xmp: Option<&'a str>,
}

/// An export target (JPEG, PNG, TIFF, ...). Resize/color/orientation/watermark are shared by every
/// format and happen before [`Exporter::encode`].
pub trait Exporter: Module {
    fn format(&self) -> ExportFormat;
    /// File extension without the dot.
    fn extension(&self) -> &'static str;
    fn supports_depth(&self, depth: BitDepth) -> bool;
    fn embeds(&self) -> EmbedSupport;
    /// Encodes `img` per `fmt` and attaches `embed`.
    fn encode(
        &self,
        img: &OutputImage<'_>,
        fmt: &FormatSpec,
        embed: &Embed<'_>,
    ) -> Result<Vec<u8>, ExportError>;
}

/// Registry of exporter modules, keyed by namespaced id.
pub type ExporterRegistry = Registry<dyn Exporter>;

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error(transparent)]
    Spec(#[from] spec::SpecError),
    #[error("bad frame: {0}")]
    Frame(String),
    #[error(transparent)]
    Resize(#[from] resize::ResizeError),
    #[error(transparent)]
    Watermark(#[from] watermark::WatermarkError),
    #[error(transparent)]
    Metadata(#[from] metadata::MetadataError),
    #[error("no exporter registered for {0:?}")]
    NoExporter(ExportFormat),
    #[error("{format:?} export doesn't support {depth:?} samples")]
    UnsupportedDepth {
        format: ExportFormat,
        depth: BitDepth,
    },
    #[error("ICC profile: {0}")]
    Icc(String),
    #[error("encode: {0}")]
    Encode(String),
}

/// Largest exported image, in pixels. Resize allocates several f32 RGB copies of the *output*
/// (12 bytes/px each), and only the render buffer uses fallible allocation, so an unbounded
/// target (`LongEdge(60000)` on a small source with "don't enlarge" off) would abort the process
/// on OOM and lose unsaved edits. 250 MP (~3 GB of f32) is far past any real print.
pub const MAX_OUTPUT_PIXELS: u64 = 250_000_000;

/// A rendered frame in host memory: interleaved **linear ProPhoto (D50)** RGB f32, unclamped --
/// what Tapetum's readback produces, alpha dropped.
pub struct WorkingFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<f32>,
}

/// A finished file, in memory.
#[derive(Debug)]
pub struct Exported {
    pub bytes: Vec<u8>,
    /// Final pixel size (after resize and orientation).
    pub width: u32,
    pub height: u32,
    pub extension: &'static str,
}

/// Everything besides the frame that one export needs.
pub struct ExportContext<'a> {
    pub spec: &'a ExportSpec,
    pub source: &'a SourceMetadata,
    /// The loaded logo, when `spec.watermark` is set (load once per batch with
    /// [`WatermarkSource::load`]).
    pub watermark: Option<&'a WatermarkSource>,
    /// Written to EXIF `Software` / XMP `CreatorTool`.
    pub software: &'a str,
}

/// Renders `frame` into a finished file: size -> resize (linear light) -> orient -> convert to the
/// output space -> watermark (linear light) -> quantize -> encode with ICC/DPI/EXIF/XMP.
pub fn export_frame(
    frame: WorkingFrame,
    ctx: &ExportContext<'_>,
    registry: &ExporterRegistry,
) -> Result<Exported, ExportError> {
    let spec = ctx.spec;
    spec.validate()?;
    let (w, h) = (frame.width, frame.height);
    if w == 0 || h == 0 || frame.pixels.len() != w as usize * h as usize * 3 {
        return Err(ExportError::Frame(format!(
            "{w}x{h} frame with {} floats",
            frame.pixels.len()
        )));
    }

    let format = spec.format.format();
    let exporter = registry
        .get(exporters::builtin_id(format))
        .ok_or(ExportError::NoExporter(format))?;
    let depth = spec.format.depth();
    if !exporter.supports_depth(depth) {
        return Err(ExportError::UnsupportedDepth { format, depth });
    }

    // Sizes: the target is chosen in *upright* space, then mapped back to the frame's own
    // (sensor) orientation so the resize runs before the rotate.
    let orientation = ctx.source.exif.orientation;
    let (ow, oh) = orientation.oriented_size(w, h);
    let (tw, th) = resize::target_size(ow, oh, &spec.resize);
    if u64::from(tw) * u64::from(th) > MAX_OUTPUT_PIXELS {
        return Err(ExportError::Frame(format!(
            "the export would be {tw}x{th} ({} MP); the limit is {} MP -- lower the size setting",
            u64::from(tw) * u64::from(th) / 1_000_000,
            MAX_OUTPUT_PIXELS / 1_000_000
        )));
    }
    let (rw, rh) = orientation.oriented_size(tw, th); // swap back if the orientation swaps

    let resized = resize::resize_linear_f32(frame.pixels, w, h, rw, rh)?;
    let mut pixels = if orientation == orient::Orientation::Normal {
        resized
    } else {
        orient::apply(&resized, rw, rh, 3, orientation).0
    };

    let space: OutputSpace = spec.color_space.into();
    color::to_output_linear(&mut pixels, space);
    match (&spec.watermark, ctx.watermark) {
        (Some(wm_spec), Some(source)) => {
            watermark::apply(&mut pixels, tw, th, source, wm_spec, space)?
        }
        (Some(_), None) => {
            return Err(ExportError::Watermark(watermark::WatermarkError::Read {
                path: "(not loaded)".into(),
                source: std::io::Error::other("spec has a watermark but none was loaded"),
            }))
        }
        (None, _) => {}
    }
    let quantized = color::quantize(&pixels, space, depth);
    drop(pixels);

    let icc =
        nicti_calico::icc::profile_bytes(space).map_err(|e| ExportError::Icc(format!("{e:?}")))?;
    let exif = metadata::build_exif(&ExifContext {
        spec: &spec.metadata,
        source: ctx.source,
        width: tw,
        height: th,
        space: spec.color_space,
        dpi: spec.dpi,
        software: ctx.software,
    });
    let xmp = metadata::build_xmp(&spec.metadata, ctx.source, ctx.software);
    let bytes = exporter.encode(
        &OutputImage {
            width: tw,
            height: th,
            pixels: &quantized,
        },
        &spec.format,
        &Embed {
            icc: &icc,
            dpi: spec.dpi,
            exif: exif.as_ref(),
            xmp: xmp.as_deref(),
        },
    )?;
    Ok(Exported {
        bytes,
        width: tw,
        height: th,
        extension: exporter.extension(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exporters::builtin_registry;
    use crate::metadata::SourceExif;
    use crate::orient::Orientation;
    use crate::spec::{
        Anchor, ExportSpace, MetadataPolicy, ResizeMode, ResizeSpec, Subsampling, TiffCompression,
        WatermarkSpec,
    };
    use nicti_claw::Descriptor;
    use serde_json::Value;
    use std::sync::Arc;

    /// Left half linear 0.05, right half linear 0.8 -- so orientation is visible in the output.
    fn split_frame(w: u32, h: u32) -> WorkingFrame {
        let mut pixels = Vec::new();
        for _y in 0..h {
            for x in 0..w {
                let v = if x < w / 2 { 0.05 } else { 0.8 };
                pixels.extend_from_slice(&[v, v, v]);
            }
        }
        WorkingFrame {
            width: w,
            height: h,
            pixels,
        }
    }

    fn ctx<'a>(spec: &'a ExportSpec, source: &'a SourceMetadata) -> ExportContext<'a> {
        ExportContext {
            spec,
            source,
            watermark: None,
            software: "Nicti test",
        }
    }

    fn decode(bytes: &[u8]) -> image::RgbImage {
        image::load_from_memory(bytes).unwrap().to_rgb8()
    }

    #[test]
    fn full_pipeline_resizes_to_the_long_edge_for_every_format() {
        let reg = builtin_registry();
        let src = SourceMetadata::default();
        for format in [
            FormatSpec::Jpeg {
                quality: 85,
                subsampling: Subsampling::S420,
            },
            FormatSpec::Png {
                depth: BitDepth::Sixteen,
            },
            FormatSpec::Tiff {
                depth: BitDepth::Eight,
                compression: TiffCompression::Deflate,
            },
        ] {
            let spec = ExportSpec {
                format,
                resize: ResizeSpec {
                    mode: ResizeMode::LongEdge(40),
                    dont_enlarge: true,
                },
                ..ExportSpec::default()
            };
            let out = export_frame(split_frame(80, 60), &ctx(&spec, &src), &reg).unwrap();
            assert_eq!((out.width, out.height), (40, 30), "{format:?}");
            let img = image::load_from_memory(&out.bytes).unwrap();
            assert_eq!((img.width(), img.height()), (40, 30));
        }
    }

    #[test]
    fn output_is_encoded_with_the_output_curve_not_linear() {
        let reg = builtin_registry();
        let spec = ExportSpec {
            format: FormatSpec::Png {
                depth: BitDepth::Eight,
            },
            ..ExportSpec::default()
        };
        let out = export_frame(
            split_frame(8, 4),
            &ctx(&spec, &SourceMetadata::default()),
            &reg,
        )
        .unwrap();
        let img = decode(&out.bytes);
        // linear 0.8 -> sRGB ~0.906 -> ~231; linear 0.05 -> ~0.248 -> ~63. (Not 204 / 13.)
        let hi = img.get_pixel(7, 2)[0] as i32;
        let lo = img.get_pixel(0, 2)[0] as i32;
        assert!((hi - 231).abs() <= 3, "{hi}");
        assert!((lo - 63).abs() <= 3, "{lo}");
    }

    #[test]
    fn orientation_is_applied_after_resize_and_size_is_computed_upright() {
        let reg = builtin_registry();
        let src = SourceMetadata {
            exif: SourceExif {
                orientation: Orientation::Rotate90Cw,
                ..SourceExif::default()
            },
            ..SourceMetadata::default()
        };
        let spec = ExportSpec {
            format: FormatSpec::Png {
                depth: BitDepth::Eight,
            },
            resize: ResizeSpec {
                mode: ResizeMode::LongEdge(40),
                dont_enlarge: true,
            },
            ..ExportSpec::default()
        };
        // Sensor frame 80x60 (landscape); rotated 90 CW it displays 60x80 (portrait) -> 30x40.
        let out = export_frame(split_frame(80, 60), &ctx(&spec, &src), &reg).unwrap();
        assert_eq!((out.width, out.height), (30, 40));
        let img = decode(&out.bytes);
        // Source left half (dark) is the top after a 90-degree CW rotation.
        assert!(img.get_pixel(15, 2)[0] < 100 && img.get_pixel(15, 37)[0] > 180);
    }

    #[test]
    fn dont_enlarge_and_a_metadata_policy_none_leave_no_exif_or_xmp() {
        let reg = builtin_registry();
        let mut spec = ExportSpec {
            format: FormatSpec::Jpeg {
                quality: 80,
                subsampling: Subsampling::S420,
            },
            resize: ResizeSpec {
                mode: ResizeMode::LongEdge(4000),
                dont_enlarge: true,
            },
            ..ExportSpec::default()
        };
        spec.metadata.policy = MetadataPolicy::None;
        let out = export_frame(
            split_frame(16, 8),
            &ctx(&spec, &SourceMetadata::default()),
            &reg,
        )
        .unwrap();
        assert_eq!((out.width, out.height), (16, 8));
        assert!(metadata::read_xmp_jpeg(&out.bytes).unwrap().is_none());
        let parsed = img_parts::jpeg::Jpeg::from_bytes(bytes::Bytes::from(out.bytes)).unwrap();
        assert!(parsed
            .segments()
            .iter()
            .all(|s| !s.contents().starts_with(b"Exif\0\0")));
    }

    #[test]
    fn a_watermark_changes_only_its_corner() {
        let reg = builtin_registry();
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="white"/></svg>"#;
        let logo = WatermarkSource::from_svg(svg.into()).unwrap();
        let spec = ExportSpec {
            format: FormatSpec::Png {
                depth: BitDepth::Eight,
            },
            watermark: Some(WatermarkSpec {
                path: "logo.svg".into(),
                anchor: Anchor::BottomRight,
                scale_pct: 25.0,
                opacity: 1.0,
                inset_pct: 0.0,
            }),
            ..ExportSpec::default()
        };
        let src = SourceMetadata::default();
        let plain = export_frame(
            split_frame(80, 40),
            &ctx(
                &ExportSpec {
                    watermark: None,
                    ..spec.clone()
                },
                &src,
            ),
            &reg,
        )
        .unwrap();
        let marked = export_frame(
            split_frame(80, 40),
            &ExportContext {
                spec: &spec,
                source: &src,
                watermark: Some(&logo),
                software: "t",
            },
            &reg,
        )
        .unwrap();
        let (a, b) = (decode(&plain.bytes), decode(&marked.bytes));
        assert_eq!(a.get_pixel(5, 5), b.get_pixel(5, 5));
        assert_eq!(b.get_pixel(75, 35)[0], 255, "logo corner is white");
        assert_ne!(a.get_pixel(75, 35), b.get_pixel(75, 35));
        // A spec that wants a watermark but was given none fails loudly instead of silently skipping.
        assert!(matches!(
            export_frame(split_frame(80, 40), &ctx(&spec, &src), &reg),
            Err(ExportError::Watermark(_))
        ));
    }

    #[test]
    fn wider_gamuts_embed_their_own_profile_and_keep_neutrals_neutral() {
        let reg = builtin_registry();
        for space in [
            ExportSpace::Srgb,
            ExportSpace::DisplayP3,
            ExportSpace::AdobeRgb,
        ] {
            let spec = ExportSpec {
                format: FormatSpec::Png {
                    depth: BitDepth::Eight,
                },
                color_space: space,
                ..ExportSpec::default()
            };
            let out = export_frame(
                split_frame(8, 4),
                &ctx(&spec, &SourceMetadata::default()),
                &reg,
            )
            .unwrap();
            let png =
                img_parts::png::Png::from_bytes(bytes::Bytes::from(out.bytes.clone())).unwrap();
            let want = nicti_calico::icc::profile_bytes(space.into()).unwrap();
            use img_parts::ImageICC;
            assert_eq!(
                png.icc_profile().unwrap().as_ref(),
                want.as_slice(),
                "{space:?}"
            );
            let px = *decode(&out.bytes).get_pixel(7, 2);
            assert!(
                px[0].abs_diff(px[1]) <= 2 && px[1].abs_diff(px[2]) <= 2,
                "{space:?} {px:?}"
            );
        }
    }

    #[test]
    fn an_absurd_output_size_is_refused_before_allocating() {
        let reg = builtin_registry();
        let spec = ExportSpec {
            resize: ResizeSpec {
                mode: ResizeMode::LongEdge(60_000),
                dont_enlarge: false,
            },
            ..ExportSpec::default()
        };
        let err = export_frame(
            split_frame(2, 2),
            &ctx(&spec, &SourceMetadata::default()),
            &reg,
        );
        assert!(matches!(err, Err(ExportError::Frame(m)) if m.contains("limit")));
    }

    #[test]
    fn bad_inputs_are_errors_not_panics() {
        let reg = builtin_registry();
        let src = SourceMetadata::default();
        let spec = ExportSpec::default();
        let bad = WorkingFrame {
            width: 4,
            height: 4,
            pixels: vec![0.0; 5],
        };
        assert!(matches!(
            export_frame(bad, &ctx(&spec, &src), &reg),
            Err(ExportError::Frame(_))
        ));
        let invalid = ExportSpec {
            dpi: 0,
            ..ExportSpec::default()
        };
        assert!(matches!(
            export_frame(split_frame(4, 4), &ctx(&invalid, &src), &reg),
            Err(ExportError::Spec(_))
        ));
        let empty: ExporterRegistry = Registry::new();
        assert!(matches!(
            export_frame(split_frame(4, 4), &ctx(&spec, &src), &empty),
            Err(ExportError::NoExporter(_))
        ));
    }

    struct Dummy;

    impl Module for Dummy {
        fn id(&self) -> &str {
            "nicti.exporter.dummy"
        }
        fn schema_version(&self) -> u32 {
            1
        }
        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }

    impl Exporter for Dummy {
        fn format(&self) -> ExportFormat {
            ExportFormat::Jpeg
        }
        fn extension(&self) -> &'static str {
            "dummy"
        }
        fn supports_depth(&self, _depth: BitDepth) -> bool {
            true
        }
        fn embeds(&self) -> EmbedSupport {
            EmbedSupport {
                exif: false,
                xmp: false,
                icc: false,
                dpi: false,
            }
        }
        fn encode(
            &self,
            _: &OutputImage<'_>,
            _: &FormatSpec,
            _: &Embed<'_>,
        ) -> Result<Vec<u8>, ExportError> {
            Ok(b"dummy".to_vec())
        }
    }

    #[test]
    fn a_third_party_exporter_registers_and_resolves_as_a_trait_object() {
        let mut registry: ExporterRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: "nicti.exporter.dummy",
                    schema_version: 1,
                },
                || Arc::new(Dummy) as Arc<dyn Exporter>,
            )
            .expect("registration should succeed");
        let resolved = registry.get("nicti.exporter.dummy").expect("registered");
        assert_eq!(resolved.id(), "nicti.exporter.dummy");
        assert_eq!(resolved.extension(), "dummy");
    }
}
