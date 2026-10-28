//! The export settings model (#57): what one export run does to every photo it's given.
//!
//! Every struct is `#[serde(default)]` and unknown fields are tolerated (serde's default), so a
//! preset file written by a newer build still loads in an older one and a preset from before a
//! field existed still loads today. [`ExportSpec::validate`] is the single gate a UI and the
//! export pipeline both call before anything is rendered.

use std::path::PathBuf;

use nicti_calico::space::OutputSpace;
use serde::{Deserialize, Serialize};

use crate::naming::Template;

/// Current schema version of a saved [`ExportPreset`].
pub const PRESET_SCHEMA_VERSION: u32 = 1;

/// Largest long-edge / short-edge / fit dimension accepted (px). A 45 MP frame is ~8300 px on its
/// long edge; this leaves room for upscaling without letting a typo ask for a terabyte buffer.
pub const MAX_DIMENSION_PX: u32 = 60_000;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SpecError {
    #[error("JPEG quality must be 1..=100, got {0}")]
    JpegQuality(u8),
    #[error("AVIF quality must be 1..=100, got {0}")]
    AvifQuality(u8),
    #[error("AVIF export supports the sRGB color space only")]
    AvifColorSpace,
    #[error("DPI must be 1..=10000, got {0}")]
    Dpi(u32),
    #[error("resize dimension must be 1..={MAX_DIMENSION_PX} px, got {0}")]
    Dimension(u32),
    #[error("megapixel target must be a finite number in (0, 1000], got {0}")]
    Megapixels(f32),
    #[error("watermark {field} must be a finite number in {range}, got {value}")]
    Watermark {
        field: &'static str,
        range: &'static str,
        value: f32,
    },
    #[error("a watermark needs a logo file or a text")]
    WatermarkPath,
    #[error("watermark text must be 1-{MAX_WATERMARK_TEXT} characters with no control characters")]
    WatermarkText,
    #[error("filename template: {0}")]
    Template(#[from] crate::naming::TemplateError),
    #[error("subfolder template: {0}")]
    Subfolder(crate::naming::TemplateError),
    #[error("fixed destination folder is empty")]
    EmptyDestination,
    #[error("sequence start must be at most 999999999, got {0}")]
    SequenceStart(u32),
}

/// A named, saved [`ExportSpec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportPreset {
    pub schema_version: u32,
    pub name: String,
    pub spec: ExportSpec,
}

impl Default for ExportPreset {
    fn default() -> Self {
        Self {
            schema_version: PRESET_SCHEMA_VERSION,
            name: "Untitled".to_string(),
            spec: ExportSpec::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportSpec {
    pub format: FormatSpec,
    pub color_space: ExportSpace,
    pub resize: ResizeSpec,
    /// Output sharpening after the resize (#339); `None` = off.
    pub sharpen: Option<crate::sharpen::SharpenSpec>,
    /// Pixels per inch written into the file. Metadata only -- never resamples.
    pub dpi: u32,
    pub metadata: MetadataSpec,
    pub watermark: Option<WatermarkSpec>,
    pub naming: NamingSpec,
    pub destination: DestinationSpec,
    pub collision: CollisionPolicy,
}

impl Default for ExportSpec {
    fn default() -> Self {
        Self {
            format: FormatSpec::default(),
            color_space: ExportSpace::default(),
            resize: ResizeSpec::default(),
            sharpen: None,
            dpi: 300,
            metadata: MetadataSpec::default(),
            watermark: None,
            naming: NamingSpec::default(),
            destination: DestinationSpec::default(),
            collision: CollisionPolicy::default(),
        }
    }
}

impl ExportSpec {
    /// Checks every field's range and parses both templates. `Err` names the first problem.
    pub fn validate(&self) -> Result<(), SpecError> {
        if let FormatSpec::Jpeg { quality, .. } = self.format {
            if !(1..=100).contains(&quality) {
                return Err(SpecError::JpegQuality(quality));
            }
        }
        if let FormatSpec::Avif { quality, .. } = self.format {
            if !(1..=100).contains(&quality) {
                return Err(SpecError::AvifQuality(quality));
            }
            // ravif can't embed ICC and only writes the implied BT.709/sRGB colour label.
            if self.color_space != ExportSpace::Srgb {
                return Err(SpecError::AvifColorSpace);
            }
        }
        if !(1..=10_000).contains(&self.dpi) {
            return Err(SpecError::Dpi(self.dpi));
        }
        match self.resize.mode {
            ResizeMode::None => {}
            ResizeMode::LongEdge(px) | ResizeMode::ShortEdge(px) => check_dimension(px)?,
            ResizeMode::Fit { width, height } => {
                check_dimension(width)?;
                check_dimension(height)?;
            }
            ResizeMode::Megapixels(mp) => {
                if !(mp.is_finite() && mp > 0.0 && mp <= 1000.0) {
                    return Err(SpecError::Megapixels(mp));
                }
            }
        }
        if let Some(w) = &self.watermark {
            w.validate()?;
        }
        Template::parse(&self.naming.template)?;
        if self.naming.sequence_start > 999_999_999 {
            return Err(SpecError::SequenceStart(self.naming.sequence_start));
        }
        if let Some(sub) = &self.destination.subfolder {
            if !sub.is_empty() {
                Template::parse_subfolder(sub).map_err(SpecError::Subfolder)?;
            }
        }
        if let DestinationBase::Folder(p) = &self.destination.base {
            if p.as_os_str().is_empty() {
                return Err(SpecError::EmptyDestination);
            }
        }
        Ok(())
    }
}

fn check_dimension(px: u32) -> Result<(), SpecError> {
    if (1..=MAX_DIMENSION_PX).contains(&px) {
        Ok(())
    } else {
        Err(SpecError::Dimension(px))
    }
}

// --- format -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Jpeg,
    Png,
    Tiff,
    Avif,
    Webp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BitDepth {
    Eight,
    Sixteen,
}

/// JPEG chroma subsampling. Always pinned explicitly: `jpeg-encoder`'s own default silently
/// switches 4:2:0 -> 4:4:4 at quality >= 90 (ADR-0056, #223).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Subsampling {
    #[default]
    S420,
    S444,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TiffCompression {
    None,
    Lzw,
    #[default]
    Deflate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FormatSpec {
    Jpeg {
        #[serde(default = "default_quality")]
        quality: u8,
        #[serde(default)]
        subsampling: Subsampling,
    },
    Png {
        #[serde(default = "default_depth")]
        depth: BitDepth,
    },
    Tiff {
        #[serde(default = "default_depth")]
        depth: BitDepth,
        #[serde(default)]
        compression: TiffCompression,
    },
    /// AV1 still. `Sixteen` is encoded as 10-bit (AVIF's practical ceiling).
    Avif {
        #[serde(default = "default_avif_quality")]
        quality: u8,
        #[serde(default = "default_depth")]
        depth: BitDepth,
    },
    /// Lossless WebP (8-bit); no quality knob until a lossy encoder exists (#361).
    Webp,
}

fn default_quality() -> u8 {
    90
}

fn default_avif_quality() -> u8 {
    80
}

fn default_depth() -> BitDepth {
    BitDepth::Eight
}

impl Default for FormatSpec {
    fn default() -> Self {
        FormatSpec::Jpeg {
            quality: default_quality(),
            subsampling: Subsampling::S420,
        }
    }
}

impl FormatSpec {
    pub fn format(&self) -> ExportFormat {
        match self {
            FormatSpec::Jpeg { .. } => ExportFormat::Jpeg,
            FormatSpec::Png { .. } => ExportFormat::Png,
            FormatSpec::Tiff { .. } => ExportFormat::Tiff,
            FormatSpec::Avif { .. } => ExportFormat::Avif,
            FormatSpec::Webp => ExportFormat::Webp,
        }
    }

    pub fn depth(&self) -> BitDepth {
        match self {
            FormatSpec::Jpeg { .. } | FormatSpec::Webp => BitDepth::Eight,
            FormatSpec::Png { depth }
            | FormatSpec::Tiff { depth, .. }
            | FormatSpec::Avif { depth, .. } => *depth,
        }
    }
}

// --- color ------------------------------------------------------------------------------------

/// The output gamut. A serde-friendly mirror of `nicti_calico::space::OutputSpace` (calico stays
/// serde-free).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExportSpace {
    #[default]
    Srgb,
    DisplayP3,
    AdobeRgb,
}

impl From<ExportSpace> for OutputSpace {
    fn from(s: ExportSpace) -> Self {
        match s {
            ExportSpace::Srgb => OutputSpace::Srgb,
            ExportSpace::DisplayP3 => OutputSpace::DisplayP3,
            ExportSpace::AdobeRgb => OutputSpace::AdobeRgb,
        }
    }
}

// --- resize -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResizeMode {
    /// Keep the rendered (cropped) size.
    #[default]
    None,
    LongEdge(u32),
    ShortEdge(u32),
    /// Fit inside a `width` x `height` box, preserving aspect ratio.
    Fit {
        width: u32,
        height: u32,
    },
    /// Scale to this many megapixels.
    Megapixels(f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResizeSpec {
    pub mode: ResizeMode,
    /// Never upscale: a photo already smaller than the target is exported at its own size.
    pub dont_enlarge: bool,
}

impl Default for ResizeSpec {
    fn default() -> Self {
        Self {
            mode: ResizeMode::None,
            dont_enlarge: true,
        }
    }
}

// --- metadata ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MetadataPolicy {
    /// Camera/exposure/date/artist/copyright EXIF plus XMP.
    #[default]
    All,
    /// Only Artist/Copyright (EXIF) and creator/rights (XMP).
    CopyrightOnly,
    /// No EXIF or XMP. The ICC profile and DPI are still written.
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MetadataSpec {
    pub policy: MetadataPolicy,
    /// Write the photo's catalog keywords into XMP (`dc:subject`). Ignored for `None`.
    pub include_keywords: bool,
    /// Write the photo's GPS position (EXIF GPS sub-IFD) when the source file has one. Off by
    /// default: a position is the most sensitive thing a photo carries, so sharing it is opt-in.
    /// Only applies under [`MetadataPolicy::All`]; `CopyrightOnly` and `None` never carry it.
    pub include_location: bool,
    pub artist: Option<String>,
    pub copyright: Option<String>,
}

// --- watermark --------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    #[default]
    BottomRight,
    Bottom,
    BottomLeft,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatermarkSpec {
    /// An `.svg` or `.png` logo. Ignored when [`Self::text`] is set.
    pub path: PathBuf,
    /// A text watermark (#338), set in the bundled Inter Regular, instead of a logo. Latin,
    /// Greek and Cyrillic only; no shaping, so no complex scripts or kerning.
    pub text: String,
    /// The text's sRGB color.
    pub text_color: [u8; 3],
    pub anchor: Anchor,
    /// Logo width as a percentage of the exported image's width.
    pub scale_pct: f32,
    /// 0.0 (invisible) ..= 1.0 (as authored).
    pub opacity: f32,
    /// Distance from the anchored edge(s), as a percentage of the image's short edge.
    pub inset_pct: f32,
}

impl Default for WatermarkSpec {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            text: String::new(),
            text_color: [255, 255, 255],
            anchor: Anchor::BottomRight,
            scale_pct: 15.0,
            opacity: 1.0,
            inset_pct: 2.0,
        }
    }
}

/// Zero-width, soft-hyphen and bidi-control characters: they draw nothing (or a `.notdef` box)
/// and can reorder what a reader sees, so a watermark may not contain them.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{ad}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}' | '\u{feff}'
    )
}

/// Longest text watermark, in characters.
pub const MAX_WATERMARK_TEXT: usize = 120;

impl WatermarkSpec {
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.text.is_empty() {
            if self.path.as_os_str().is_empty() {
                return Err(SpecError::WatermarkPath);
            }
        } else if self.text.trim().is_empty()
            || self.text.chars().count() > MAX_WATERMARK_TEXT
            || self
                .text
                .chars()
                .any(|c| c.is_control() || is_invisible_format(c))
        {
            return Err(SpecError::WatermarkText);
        }
        let check = |field, range, value: f32, lo: f32, hi: f32| {
            if value.is_finite() && value > lo && value <= hi {
                Ok(())
            } else {
                Err(SpecError::Watermark {
                    field,
                    range,
                    value,
                })
            }
        };
        check("scale", "(0, 100]", self.scale_pct, 0.0, 100.0)?;
        check("opacity", "(0, 1]", self.opacity, 0.0, 1.0)?;
        if !(self.inset_pct.is_finite() && (0.0..=50.0).contains(&self.inset_pct)) {
            return Err(SpecError::Watermark {
                field: "inset",
                range: "[0, 50]",
                value: self.inset_pct,
            });
        }
        Ok(())
    }
}

// --- naming / destination ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NamingSpec {
    /// Filename template, without extension. See [`crate::naming`] for the token grammar.
    pub template: String,
    /// First `{Sequence}` value.
    pub sequence_start: u32,
}

impl Default for NamingSpec {
    fn default() -> Self {
        Self {
            template: "{Filename}".to_string(),
            sequence_start: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DestinationBase {
    /// Next to each source file. Safe: Scruff only imports RAW extensions, so an export can never
    /// be mistaken for (or overwrite) a catalog asset.
    #[default]
    SameAsSource,
    Folder(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DestinationSpec {
    pub base: DestinationBase,
    /// Optional subfolder under `base`, itself a token template (`{Date:YYYY}/{Folder}`); `/` is
    /// allowed here (and only here) as a separator.
    pub subfolder: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CollisionPolicy {
    /// `name-2.jpg`, `name-3.jpg`, ... -- never touches an existing file.
    #[default]
    UniqueSuffix,
    Overwrite,
    Skip,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_spec_is_valid_and_round_trips_through_json() {
        let spec = ExportSpec::default();
        spec.validate().unwrap();
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<ExportSpec>(&json).unwrap(), spec);
    }

    #[test]
    fn an_empty_or_partial_json_object_fills_in_defaults() {
        let spec: ExportSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(spec.naming.template, "{Filename}");
        assert!(spec.resize.dont_enlarge);
        let spec: ExportSpec =
            serde_json::from_str(r#"{"format":{"kind":"png"},"future_field":1}"#).unwrap();
        assert_eq!(
            spec.format,
            FormatSpec::Png {
                depth: BitDepth::Eight
            }
        );
    }

    #[test]
    fn validate_rejects_out_of_range_values() {
        let good = ExportSpec::default();
        let mut s = good.clone();
        s.format = FormatSpec::Jpeg {
            quality: 0,
            subsampling: Subsampling::S420,
        };
        assert_eq!(s.validate(), Err(SpecError::JpegQuality(0)));
        let mut s = good.clone();
        s.format = FormatSpec::Avif {
            quality: 101,
            depth: BitDepth::Eight,
        };
        assert_eq!(s.validate(), Err(SpecError::AvifQuality(101)));
        s.format = FormatSpec::Avif {
            quality: 80,
            depth: BitDepth::Eight,
        };
        assert_eq!(s.validate(), Ok(()));
        s.color_space = ExportSpace::DisplayP3;
        assert_eq!(s.validate(), Err(SpecError::AvifColorSpace));
        let mut s = good.clone();
        s.dpi = 0;
        assert_eq!(s.validate(), Err(SpecError::Dpi(0)));
        let mut s = good.clone();
        s.resize.mode = ResizeMode::LongEdge(0);
        assert_eq!(s.validate(), Err(SpecError::Dimension(0)));
        s.resize.mode = ResizeMode::LongEdge(MAX_DIMENSION_PX + 1);
        assert!(matches!(s.validate(), Err(SpecError::Dimension(_))));
        s.resize.mode = ResizeMode::Megapixels(f32::NAN);
        assert!(matches!(s.validate(), Err(SpecError::Megapixels(_))));
        let mut s = good.clone();
        s.watermark = Some(WatermarkSpec::default()); // empty path
        assert_eq!(s.validate(), Err(SpecError::WatermarkPath));
        // Text instead of a path is valid; control characters and over-long text are not.
        s.watermark = Some(WatermarkSpec {
            text: "(c) Jo".into(),
            ..WatermarkSpec::default()
        });
        assert_eq!(s.validate(), Ok(()));
        for bad in [
            "a\nb",
            "   ",
            "\u{200b}",
            "\u{202e}ab",
            &"x".repeat(MAX_WATERMARK_TEXT + 1),
        ] {
            s.watermark = Some(WatermarkSpec {
                text: bad.to_string(),
                ..WatermarkSpec::default()
            });
            assert_eq!(s.validate(), Err(SpecError::WatermarkText), "{bad:?}");
        }
        s.watermark = Some(WatermarkSpec {
            path: "logo.png".into(),
            opacity: 1.5,
            ..WatermarkSpec::default()
        });
        assert!(matches!(s.validate(), Err(SpecError::Watermark { .. })));
        let mut s = good.clone();
        s.naming.template = "{Bogus}".into();
        assert!(matches!(s.validate(), Err(SpecError::Template(_))));
        let mut s = good.clone();
        s.destination.base = DestinationBase::Folder(PathBuf::new());
        assert_eq!(s.validate(), Err(SpecError::EmptyDestination));
        let mut s = good;
        s.destination.subfolder = Some("{Nope}".into());
        assert!(matches!(s.validate(), Err(SpecError::Subfolder(_))));
    }
}
