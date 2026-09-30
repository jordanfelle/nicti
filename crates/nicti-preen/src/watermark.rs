//! Logo watermark (#57, promoted from `spikes/prey`; ADR-0056).
//!
//! An SVG or PNG logo is composited onto the **linear output-space** frame with "over" in linear
//! light -- straight-alpha blending of gamma-encoded values darkens semi-transparent edges the same
//! way a gamma-space resize does. The logo's colors are sRGB by convention; they are converted to
//! linear sRGB and then into the export space through the working-space matrices, so a logo keeps
//! its look in Display P3 / Adobe RGB output.
//!
//! Text watermarks are deferred (they need a vendored font with a confirmed license).

use std::path::Path;

use image::{imageops, Rgba, RgbaImage};
use nicti_calico::space::OutputSpace;

use crate::color::srgb_to_output_matrix;
use crate::spec::{Anchor, WatermarkSpec};

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

    /// Intrinsic aspect ratio (height / width).
    fn aspect(&self) -> f64 {
        match self {
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

    // tiny-skia stores premultiplied alpha; un-premultiply so an SVG and a PNG logo enter the
    // compositor in the same representation.
    let mut out = RgbaImage::new(width, height);
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
    Ok(out)
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
}
