//! Logo watermark (#57, promoted from `spikes/prey`; ADR-0056).
//!
//! An SVG or PNG logo is composited onto the **linear output-space** frame with "over" in linear
//! light -- straight-alpha blending of gamma-encoded values darkens semi-transparent edges the same
//! way a gamma-space resize does. The logo's colors are sRGB by convention; they are converted to
//! linear sRGB and then into the export space through the working-space matrices, so a logo keeps
//! its look in Display P3 / Adobe RGB output.
//!
//! A text watermark (#338) is set in the bundled Inter Regular (SIL OFL 1.1, already vendored for
//! the UI; see `assets/ATTRIBUTION.md`): glyph outlines from `skrifa`, filled with `tiny-skia`,
//! then composited exactly like a logo. There is no shaping, so Latin/Greek/Cyrillic only, no
//! kerning and no complex scripts; a character the font lacks draws its `.notdef` box.

use std::path::Path;

use image::{imageops, Rgba, RgbaImage};
use nicti_calico::space::OutputSpace;

use crate::color::srgb_to_output_matrix;
use crate::spec::{Anchor, WatermarkSpec};

/// Inter Regular, SIL OFL 1.1. The one copy in the repo lives with the UI fonts.
const INTER_REGULAR: &[u8] = include_bytes!("../../nicti-pelt/assets/fonts/Inter-Regular.ttf");

#[derive(Debug, thiserror::Error)]
pub enum WatermarkError {
    #[error("reading watermark {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("watermark must be an .svg or .png file (got {0:?})")]
    UnsupportedType(String),
    #[error("decoding watermark PNG: {0}")]
    Png(String),
    #[error("parsing watermark SVG: {0}")]
    Svg(String),
    #[error("text watermark: {0}")]
    Text(String),
    #[error("watermark has zero size")]
    Empty,
}

/// A decoded logo, ready to be scaled per photo. Load once per batch and share.
#[derive(Debug, Clone)]
pub enum WatermarkSource {
    /// `aspect` (height / width) is computed once in `from_svg`, which also validates the text.
    Svg {
        text: String,
        aspect: f64,
    },
    Png(RgbaImage),
    /// Laid-out text: glyph outlines in font units, with `width` x `height` of the line box.
    Text(Box<TextLayout>),
}

/// Padding around the ink of a text watermark, in em.
const TEXT_PAD_EM: f32 = 0.03;

/// A line of text as one filled path, in font units with y down from the top of the line box.
#[derive(Debug, Clone)]
pub struct TextLayout {
    path: tiny_skia::Path,
    width: f32,
    height: f32,
    color: [u8; 3],
}

impl WatermarkSource {
    /// Reads and validates a logo file. Fails early (at plan time) instead of once per photo.
    pub fn load(path: &Path) -> Result<Self, WatermarkError> {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let read_err = |source| WatermarkError::Read {
            path: path.display().to_string(),
            source,
        };
        match ext.as_str() {
            "svg" => {
                let text = std::fs::read_to_string(path).map_err(read_err)?;
                Self::from_svg(text)
            }
            "png" => {
                let bytes = std::fs::read(path).map_err(read_err)?;
                Self::from_png(&bytes)
            }
            other => Err(WatermarkError::UnsupportedType(other.to_string())),
        }
    }

    pub fn from_svg(text: String) -> Result<Self, WatermarkError> {
        let tree = usvg::Tree::from_str(&text, &svg_options())
            .map_err(|e| WatermarkError::Svg(e.to_string()))?;
        let size = tree.size();
        if !(size.width() > 0.0 && size.height() > 0.0) {
            return Err(WatermarkError::Empty);
        }
        let aspect = f64::from(size.height()) / f64::from(size.width());
        Ok(WatermarkSource::Svg { text, aspect })
    }

    pub fn from_png(bytes: &[u8]) -> Result<Self, WatermarkError> {
        let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
            .map_err(|e| WatermarkError::Png(e.to_string()))?
            .to_rgba8();
        if img.width() == 0 || img.height() == 0 {
            return Err(WatermarkError::Empty);
        }
        Ok(WatermarkSource::Png(img))
    }

    /// The source a spec asks for: its text if it has one, else the logo file.
    pub fn for_spec(spec: &WatermarkSpec) -> Result<Self, WatermarkError> {
        if spec.text.is_empty() {
            Self::load(&spec.path)
        } else {
            Self::from_text(&spec.text, spec.text_color)
        }
    }

    /// Lays out `text` in the bundled font. Fails on an empty string or one with no ink.
    pub fn from_text(text: &str, color: [u8; 3]) -> Result<Self, WatermarkError> {
        layout_text(text, color).map(|l| WatermarkSource::Text(Box::new(l)))
    }

    /// Intrinsic aspect ratio (height / width).
    fn aspect(&self) -> f64 {
        match self {
            WatermarkSource::Text(t) => f64::from(t.height) / f64::from(t.width),
            WatermarkSource::Png(img) => img.height() as f64 / img.width() as f64,
            WatermarkSource::Svg { aspect, .. } => *aspect,
        }
    }

    /// The logo at exactly `width` x `height` px, straight-alpha sRGB.
    fn render(&self, width: u32, height: u32) -> Result<RgbaImage, WatermarkError> {
        match self {
            WatermarkSource::Png(img) => {
                if img.dimensions() == (width, height) {
                    Ok(img.clone())
                } else {
                    Ok(imageops::resize(
                        img,
                        width,
                        height,
                        imageops::FilterType::Lanczos3,
                    ))
                }
            }
            WatermarkSource::Svg { text, .. } => rasterize_svg(text, width, height),
            WatermarkSource::Text(t) => rasterize_text(t, width, height),
        }
    }
}

/// Parse options for a logo: an `<image xlink:href>` may only be an inline `data:` URL. usvg's
/// default also reads arbitrary local paths, which would let a logo file pull other files' pixels
/// into every export.
fn svg_options() -> usvg::Options<'static> {
    let mut options = usvg::Options::default();
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    options
}

/// Rasterizes an SVG at `width` x `height` into straight-alpha RGBA (sRGB-encoded color).
pub fn rasterize_svg(svg: &str, width: u32, height: u32) -> Result<RgbaImage, WatermarkError> {
    let tree = usvg::Tree::from_str(svg, &svg_options())
        .map_err(|e| WatermarkError::Svg(e.to_string()))?;
    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or(WatermarkError::Empty)?;
    let size = tree.size();
    let transform = tiny_skia::Transform::from_scale(
        width as f32 / size.width(),
        height as f32 / size.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    Ok(pixmap_to_rgba(&pixmap))
}

/// tiny-skia stores premultiplied alpha; un-premultiply so an SVG, text and a PNG logo enter the
/// compositor in the same representation.
fn pixmap_to_rgba(pixmap: &tiny_skia::Pixmap) -> RgbaImage {
    let mut out = RgbaImage::new(pixmap.width(), pixmap.height());
    for (dst, px) in out.pixels_mut().zip(pixmap.pixels()) {
        let a = px.alpha();
        *dst = if a == 0 {
            Rgba([0, 0, 0, 0])
        } else {
            let unpremul = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
            Rgba([
                unpremul(px.red()),
                unpremul(px.green()),
                unpremul(px.blue()),
                a,
            ])
        };
    }
    out
}

/// Collects a glyph outline into a tiny-skia path, offset by the pen position and flipped to y-down.
struct GlyphPen<'a> {
    builder: &'a mut tiny_skia::PathBuilder,
    x: f32,
    ascent: f32,
}

impl GlyphPen<'_> {
    fn pt(&self, x: f32, y: f32) -> (f32, f32) {
        (self.x + x, self.ascent - y)
    }
}

impl skrifa::outline::OutlinePen for GlyphPen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.pt(x, y);
        self.builder.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.pt(x, y);
        self.builder.line_to(x, y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let ((cx, cy), (x, y)) = (self.pt(cx, cy), self.pt(x, y));
        self.builder.quad_to(cx, cy, x, y);
    }
    fn curve_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        let ((a, b), (c, d), (x, y)) = (self.pt(c1x, c1y), self.pt(c2x, c2y), self.pt(x, y));
        self.builder.cubic_to(a, b, c, d, x, y);
    }
    fn close(&mut self) {
        self.builder.close();
    }
}

fn layout_text(text: &str, color: [u8; 3]) -> Result<TextLayout, WatermarkError> {
    use skrifa::instance::{LocationRef, Size};
    use skrifa::outline::DrawSettings;
    use skrifa::{FontRef, MetadataProvider};

    let bad = |why: &str| WatermarkError::Text(why.to_string());
    if text.is_empty() {
        return Err(bad("the text is empty"));
    }
    let font = FontRef::new(INTER_REGULAR).map_err(|e| bad(&e.to_string()))?;
    let loc = LocationRef::default();
    let metrics = font.metrics(Size::unscaled(), loc);
    let ascent = metrics.ascent;
    let charmap = font.charmap();
    let glyph_metrics = font.glyph_metrics(Size::unscaled(), loc);
    let outlines = font.outline_glyphs();

    let mut builder = tiny_skia::PathBuilder::new();
    let mut x = 0.0f32;
    for ch in text.chars() {
        let gid = charmap.map(ch).unwrap_or_default();
        if let Some(glyph) = outlines.get(gid) {
            let mut pen = GlyphPen {
                builder: &mut builder,
                x,
                ascent,
            };
            glyph
                .draw(DrawSettings::unhinted(Size::unscaled(), loc), &mut pen)
                .map_err(|e| bad(&e.to_string()))?;
        }
        x += glyph_metrics.advance_width(gid).unwrap_or(0.0);
    }
    let path = builder
        .finish()
        .ok_or_else(|| bad("the text has no visible glyphs"))?;
    // The box is the *ink*, plus a little padding: leading/trailing spaces and the font's line
    // metrics must not shift the visible text inside its box (anchoring would look off by the
    // space), and tall accents or deep descenders must not be clipped by it.
    let bounds = path.bounds();
    let pad = metrics.units_per_em as f32 * TEXT_PAD_EM;
    let path = path
        .transform(tiny_skia::Transform::from_translate(
            pad - bounds.left(),
            pad - bounds.top(),
        ))
        .ok_or_else(|| bad("the text could not be placed"))?;
    let (width, height) = (bounds.width() + 2.0 * pad, bounds.height() + 2.0 * pad);
    if !(width > 0.0 && height > 0.0) {
        return Err(WatermarkError::Empty);
    }
    Ok(TextLayout {
        path,
        width,
        height,
        color,
    })
}

/// Fills the laid-out text at `width` x `height` into straight-alpha RGBA (sRGB-encoded color).
fn rasterize_text(t: &TextLayout, width: u32, height: u32) -> Result<RgbaImage, WatermarkError> {
    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or(WatermarkError::Empty)?;
    let mut paint = tiny_skia::Paint::default();
    paint.set_color_rgba8(t.color[0], t.color[1], t.color[2], 255);
    paint.anti_alias = true;
    let transform =
        tiny_skia::Transform::from_scale(width as f32 / t.width, height as f32 / t.height);
    pixmap.fill_path(
        &t.path,
        &paint,
        tiny_skia::FillRule::Winding,
        transform,
        None,
    );
    Ok(pixmap_to_rgba(&pixmap))
}

fn srgb_eotf(encoded: f32) -> f32 {
    if encoded <= 0.040_45 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

/// Top-left placement of a `logo_w` x `logo_h` box in an `img_w` x `img_h` image.
pub fn place(
    anchor: Anchor,
    img_w: u32,
    img_h: u32,
    logo_w: u32,
    logo_h: u32,
    inset_px: u32,
) -> (i64, i64) {
    let (iw, ih, lw, lh, inset) = (
        img_w as i64,
        img_h as i64,
        logo_w as i64,
        logo_h as i64,
        inset_px as i64,
    );
    let x = match anchor {
        Anchor::TopLeft | Anchor::Left | Anchor::BottomLeft => inset,
        Anchor::Top | Anchor::Center | Anchor::Bottom => (iw - lw) / 2,
        Anchor::TopRight | Anchor::Right | Anchor::BottomRight => iw - lw - inset,
    };
    let y = match anchor {
        Anchor::TopLeft | Anchor::Top | Anchor::TopRight => inset,
        Anchor::Left | Anchor::Center | Anchor::Right => (ih - lh) / 2,
        Anchor::BottomLeft | Anchor::Bottom | Anchor::BottomRight => ih - lh - inset,
    };
    (x, y)
}

/// Composites `source` per `spec` onto `base` (interleaved **linear output-space** RGB, `w` x `h`),
/// in place.
pub fn apply(
    base: &mut [f32],
    w: u32,
    h: u32,
    source: &WatermarkSource,
    spec: &WatermarkSpec,
    space: OutputSpace,
) -> Result<(), WatermarkError> {
    // Logo size: `scale_pct` of the image width, shrunk to fit if it would be taller than the image.
    let aspect = source.aspect();
    let mut lw = ((w as f64 * spec.scale_pct as f64 / 100.0).round() as u32).max(1);
    let mut lh = ((lw as f64 * aspect).round() as u32).max(1);
    if lh > h {
        lh = h.max(1);
        lw = ((lh as f64 / aspect).round() as u32).max(1);
    }
    let logo = source.render(lw, lh)?;
    let inset = (w.min(h) as f64 * spec.inset_pct as f64 / 100.0).round() as u32;
    let (x0, y0) = place(spec.anchor, w, h, lw, lh, inset);
    composite(base, w, h, &logo, x0, y0, spec.opacity, space);
    Ok(())
}

/// "Over" in linear light: `out = logo * a + base * (1 - a)`, with `a = alpha * opacity`.
#[allow(clippy::too_many_arguments)]
pub fn composite(
    base: &mut [f32],
    w: u32,
    h: u32,
    logo: &RgbaImage,
    x0: i64,
    y0: i64,
    opacity: f32,
    space: OutputSpace,
) {
    let m = srgb_to_output_matrix(space);
    for (lx, ly, px) in logo.enumerate_pixels() {
        let (x, y) = (x0 + lx as i64, y0 + ly as i64);
        if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
            continue;
        }
        let a = px[3] as f32 / 255.0 * opacity;
        if a <= 0.0 {
            continue;
        }
        let lin = [
            srgb_eotf(px[0] as f32 / 255.0),
            srgb_eotf(px[1] as f32 / 255.0),
            srgb_eotf(px[2] as f32 / 255.0),
        ];
        let i = (y as usize * w as usize + x as usize) * 3;
        for c in 0..3 {
            let logo_c = m[c][0] * lin[0] + m[c][1] * lin[1] + m[c][2] * lin[2];
            base[i + c] = logo_c * a + base[i + c] * (1.0 - a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{quantize, OutputPixels};
    use crate::spec::BitDepth;

    const WHITE_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="white"/></svg>"#;

    fn spec(anchor: Anchor, scale: f32, opacity: f32, inset: f32) -> WatermarkSpec {
        WatermarkSpec {
            path: "logo.svg".into(),
            anchor,
            scale_pct: scale,
            opacity,
            inset_pct: inset,
            ..WatermarkSpec::default()
        }
    }

    fn at(base: &[f32], w: u32, x: u32, y: u32) -> f32 {
        base[(y as usize * w as usize + x as usize) * 3]
    }

    #[test]
    fn an_svg_cannot_pull_in_local_files() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.png");
        RgbaImage::from_pixel(4, 4, Rgba([255, 0, 0, 255]))
            .save(&secret)
            .unwrap();
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="4" height="4"><image xlink:href="{}" width="4" height="4"/></svg>"#,
            secret.display()
        );
        let img = rasterize_svg(&svg, 4, 4).unwrap();
        assert!(
            img.pixels().all(|p| p[3] == 0),
            "the local file was drawn: {:?}",
            img.get_pixel(1, 1)
        );
    }

    #[test]
    fn placement_for_every_anchor() {
        // 100x50 image, 20x10 logo, 5px inset.
        let p = |a| place(a, 100, 50, 20, 10, 5);
        assert_eq!(p(Anchor::TopLeft), (5, 5));
        assert_eq!(p(Anchor::Top), (40, 5));
        assert_eq!(p(Anchor::TopRight), (75, 5));
        assert_eq!(p(Anchor::Left), (5, 20));
        assert_eq!(p(Anchor::Center), (40, 20));
        assert_eq!(p(Anchor::Right), (75, 20));
        assert_eq!(p(Anchor::BottomLeft), (5, 35));
        assert_eq!(p(Anchor::Bottom), (40, 35));
        assert_eq!(p(Anchor::BottomRight), (75, 35));
    }

    #[test]
    fn an_opaque_logo_replaces_pixels_inside_its_box_only() {
        let (w, h) = (100u32, 50u32);
        let mut base = vec![0.0f32; (w * h * 3) as usize];
        let src = WatermarkSource::from_svg(WHITE_SVG.into()).unwrap();
        // 20% of 100 = 20px wide, aspect 0.5 -> 10px tall, bottom-right with a 2px (4% of 50) inset.
        apply(
            &mut base,
            w,
            h,
            &src,
            &spec(Anchor::BottomRight, 20.0, 1.0, 4.0),
            OutputSpace::Srgb,
        )
        .unwrap();
        assert!(
            (at(&base, w, 90, 40) - 1.0).abs() < 0.01,
            "inside the logo box"
        );
        assert_eq!(at(&base, w, 10, 10), 0.0, "far from the logo");
        assert_eq!(at(&base, w, 99, 49), 0.0, "inset margin stays untouched");
    }

    #[test]
    fn half_alpha_white_over_black_is_brighter_in_linear_light_than_a_gamma_average() {
        // ADR-0056's regression: linear 0.5 encodes to ~187 in 8-bit sRGB; a naive gamma-space
        // average of 0 and 255 would give 128.
        let (w, h) = (4u32, 2u32);
        let mut base = vec![0.0f32; (w * h * 3) as usize];
        let src = WatermarkSource::from_svg(WHITE_SVG.into()).unwrap();
        apply(
            &mut base,
            w,
            h,
            &src,
            &spec(Anchor::TopLeft, 100.0, 0.5, 0.0),
            OutputSpace::Srgb,
        )
        .unwrap();
        let OutputPixels::Rgb8(q) = quantize(&base[..3], OutputSpace::Srgb, BitDepth::Eight) else {
            panic!()
        };
        assert!((q[0] as i32 - 188).abs() <= 2, "{q:?}");
    }

    #[test]
    fn zero_alpha_pixels_and_out_of_frame_parts_are_left_alone_and_never_panic() {
        let (w, h) = (10u32, 10u32);
        let mut base = vec![0.25f32; (w * h * 3) as usize];
        let logo = RgbaImage::from_pixel(6, 6, Rgba([255, 255, 255, 0]));
        composite(&mut base, w, h, &logo, 2, 2, 1.0, OutputSpace::Srgb);
        assert!(base.iter().all(|&v| v == 0.25));
        // Partly off every edge, including negative origins.
        let opaque = RgbaImage::from_pixel(6, 6, Rgba([255, 255, 255, 255]));
        composite(&mut base, w, h, &opaque, -3, -3, 1.0, OutputSpace::Srgb);
        composite(&mut base, w, h, &opaque, 8, 8, 1.0, OutputSpace::Srgb);
        assert!((at(&base, w, 0, 0) - 1.0).abs() < 0.01);
        assert!((at(&base, w, 9, 9) - 1.0).abs() < 0.01);
        assert_eq!(at(&base, w, 5, 5), 0.25);
    }

    #[test]
    fn a_tall_logo_is_shrunk_to_fit_the_image_height() {
        let tall = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="100"><rect width="10" height="100" fill="white"/></svg>"#;
        let (w, h) = (100u32, 20u32);
        let mut base = vec![0.0f32; (w * h * 3) as usize];
        let src = WatermarkSource::from_svg(tall.into()).unwrap();
        apply(
            &mut base,
            w,
            h,
            &src,
            &spec(Anchor::Center, 50.0, 1.0, 0.0),
            OutputSpace::Srgb,
        )
        .unwrap();
        assert!(at(&base, w, 50, 10) > 0.9);
    }

    #[test]
    fn loads_svg_and_png_from_disk_and_rejects_other_types() {
        let dir = tempfile::tempdir().unwrap();
        let svg = dir.path().join("l.svg");
        std::fs::write(&svg, WHITE_SVG).unwrap();
        assert!(matches!(
            WatermarkSource::load(&svg).unwrap(),
            WatermarkSource::Svg { .. }
        ));

        let png = dir.path().join("l.PNG");
        let mut bytes = Vec::new();
        RgbaImage::from_pixel(4, 2, Rgba([255, 0, 0, 255]))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        std::fs::write(&png, &bytes).unwrap();
        assert!(matches!(
            WatermarkSource::load(&png).unwrap(),
            WatermarkSource::Png(_)
        ));

        let jpg = dir.path().join("l.jpg");
        std::fs::write(&jpg, b"x").unwrap();
        assert!(matches!(
            WatermarkSource::load(&jpg),
            Err(WatermarkError::UnsupportedType(_))
        ));
        assert!(matches!(
            WatermarkSource::load(&dir.path().join("missing.svg")),
            Err(WatermarkError::Read { .. })
        ));
        std::fs::write(dir.path().join("bad.svg"), "not svg").unwrap();
        assert!(matches!(
            WatermarkSource::load(&dir.path().join("bad.svg")),
            Err(WatermarkError::Svg(_))
        ));
        std::fs::write(dir.path().join("bad.png"), "not png").unwrap();
        assert!(matches!(
            WatermarkSource::load(&dir.path().join("bad.png")),
            Err(WatermarkError::Png(_))
        ));
    }

    #[test]
    fn a_red_png_logo_keeps_its_hue_in_a_wider_gamut() {
        let mut png = Vec::new();
        RgbaImage::from_pixel(2, 2, Rgba([255, 0, 0, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let src = WatermarkSource::from_png(&png).unwrap();
        let (w, h) = (8u32, 8u32);
        let mut base = vec![0.0f32; (w * h * 3) as usize];
        apply(
            &mut base,
            w,
            h,
            &src,
            &spec(Anchor::TopLeft, 100.0, 1.0, 0.0),
            OutputSpace::AdobeRgb,
        )
        .unwrap();
        // sRGB red in Adobe RGB is R < 1 with a little G, no B.
        let (r, g, b) = (base[0], base[1], base[2]);
        assert!(
            r > 0.6 && r < 1.0 && g > -1e-3 && g < 0.3 && b.abs() < 0.05,
            "{r} {g} {b}"
        );
    }

    #[test]
    fn text_renders_ink_in_the_requested_color_and_leaves_the_rest_clear() {
        let src = WatermarkSource::from_text("(c) Jo Photographer 2026", [200, 30, 40]).unwrap();
        let w = 400;
        let h = (f64::from(w) * src.aspect()).round() as u32;
        let img = src.render(w, h).unwrap();
        let inked: Vec<_> = img.pixels().filter(|p| p[3] > 200).collect();
        assert!(inked.len() > 300, "{} opaque pixels", inked.len());
        // Un-premultiplying a partly covered edge pixel rounds by a level or two.
        let near = |a: u8, b: u8| a.abs_diff(b) <= 3;
        assert!(inked
            .iter()
            .all(|p| near(p[0], 200) && near(p[1], 30) && near(p[2], 40)));
        assert!(
            img.pixels().any(|p| p[3] == 0),
            "background stays transparent"
        );
        // Roughly a line of text: wider than tall, ink covers well under half the box.
        assert!(src.aspect() < 0.2, "aspect {}", src.aspect());
        assert!(inked.len() < (w * h) as usize / 2);
    }

    #[test]
    fn longer_text_is_wider_and_different_text_looks_different() {
        let short = WatermarkSource::from_text("Jo", [255; 3]).unwrap();
        let long = WatermarkSource::from_text("Jo Photographer", [255; 3]).unwrap();
        assert!(long.aspect() < short.aspect());
        let a = short
            .render(120, (120.0 * short.aspect()).round() as u32)
            .unwrap();
        let b = WatermarkSource::from_text("Ox", [255; 3])
            .unwrap()
            .render(120, (120.0 * short.aspect()).round() as u32)
            .unwrap();
        assert_ne!(a.as_raw(), b.as_raw());
    }

    #[test]
    fn text_handles_latin1_symbols_unmapped_characters_and_rejects_nothing_to_draw() {
        assert!(WatermarkSource::from_text("© Zoë Ünï", [255; 3]).is_ok());
        // A CJK character the font lacks draws .notdef instead of failing the export.
        assert!(WatermarkSource::from_text("日本", [255; 3]).is_ok());
        assert!(matches!(
            WatermarkSource::from_text("", [255; 3]),
            Err(WatermarkError::Text(_))
        ));
        assert!(matches!(
            WatermarkSource::from_text("   ", [255; 3]),
            Err(WatermarkError::Text(_)) | Err(WatermarkError::Empty)
        ));
    }

    #[test]
    fn a_text_spec_composites_at_its_anchor_only() {
        let spec = WatermarkSpec {
            text: "Jo".into(),
            text_color: [255, 255, 255],
            anchor: Anchor::BottomRight,
            scale_pct: 20.0,
            opacity: 1.0,
            inset_pct: 2.0,
            ..WatermarkSpec::default()
        };
        assert!(spec.validate().is_ok());
        let src = WatermarkSource::for_spec(&spec).unwrap();
        let (w, h) = (200u32, 100u32);
        let mut base = vec![0.0f32; (w * h * 3) as usize];
        apply(&mut base, w, h, &src, &spec, OutputSpace::Srgb).unwrap();
        let lit = |x: u32, y: u32| base[((y * w + x) * 3) as usize] > 0.01;
        let top_left_lit = (0..w / 2).any(|x| (0..h / 2).any(|y| lit(x, y)));
        let bottom_right_lit = (w / 2..w).any(|x| (h / 2..h).any(|y| lit(x, y)));
        assert!(!top_left_lit, "text must not spill outside its anchor");
        assert!(
            bottom_right_lit,
            "text must reach the bottom-right corner area"
        );
    }

    #[test]
    fn text_is_right_side_up_and_left_to_right() {
        // A "T" has its bar at the top: more ink in the top third than the bottom third.
        let t = WatermarkSource::from_text("T", [255; 3]).unwrap();
        let (w, h) = (90u32, (90.0 * t.aspect()).round() as u32);
        let img = t.render(w, h).unwrap();
        let ink = |y0: u32, y1: u32| -> u32 {
            (y0..y1)
                .flat_map(|y| (0..w).map(move |x| (x, y)))
                .map(|(x, y)| u32::from(img.get_pixel(x, y)[3]))
                .sum()
        };
        assert!(
            ink(0, h / 3) > 2 * ink(2 * h / 3, h),
            "top {} bottom {}",
            ink(0, h / 3),
            ink(2 * h / 3, h)
        );
        // "Ix" vs "xI": the thin "I" sits on the left in the first and on the right in the second.
        let render = |s: &str| {
            let src = WatermarkSource::from_text(s, [255; 3]).unwrap();
            let h = (120.0 * src.aspect()).round() as u32;
            src.render(120, h).unwrap()
        };
        let col_ink = |img: &RgbaImage, x0: u32, x1: u32| -> u32 {
            (x0..x1)
                .flat_map(|x| (0..img.height()).map(move |y| (x, y)))
                .map(|(x, y)| u32::from(img.get_pixel(x, y)[3]))
                .sum()
        };
        let a = render("IxxxxxxX");
        let b = render("XxxxxxxI");
        assert!(col_ink(&a, 0, 12) > 0 && col_ink(&b, 108, 120) > 0);
        assert_ne!(a.as_raw(), b.as_raw());
    }

    #[test]
    fn the_box_follows_the_ink_so_spaces_do_not_shift_it_and_accents_are_not_clipped() {
        // Leading/trailing spaces change nothing.
        let plain = WatermarkSource::from_text("ab", [255; 3]).unwrap();
        let spaced = WatermarkSource::from_text("   ab    ", [255; 3]).unwrap();
        assert!((plain.aspect() - spaced.aspect()).abs() < 1e-6);
        // Tall accents and descenders stay inside the box: ink reaches (nearly) every edge but
        // is never cut by it, so the first and last rows carry only padding-level ink.
        let src = WatermarkSource::from_text("gypqj(\u{c5})", [255; 3]).unwrap();
        let w = 800;
        let h = (f64::from(w) * src.aspect()).round() as u32;
        let img = src.render(w, h).unwrap();
        let row = |y: u32| {
            (0..w)
                .map(|x| u32::from(img.get_pixel(x, y)[3]))
                .sum::<u32>()
        };
        assert_eq!(row(0), 0, "padding row above the accent");
        assert_eq!(row(h - 1), 0, "padding row below the descender");
        assert!(row(h / 10) > 0 || row(h / 5) > 0, "ink near the top");
    }
}
