//! Watermark compositing. Decision rule (`docs/adr/0056`): SVG and PNG logos, alpha-composited
//! in **linear premultiplied** light -- straight (non-premultiplied) gamma-space alpha blending
//! darkens semi-transparent edges the same way a naive resize does (see [`crate::resize`]'s doc).
//!
//! Text watermarking (a rasterized string, not a pre-made logo asset) was not reached this pass
//! -- it needs a vendored font with a confirmed license, which this pass didn't pick one for. See
//! the ADR's "what wasn't reachable" section and the follow-up issue. `resvg`/`usvg`/`tiny-skia`
//! (SVG rasterization) and the CPU/GPU composite paths below are real and tested.

use image::{Rgba, RgbaImage};

use crate::resize::{srgb_eotf, srgb_oetf};

/// Rasterizes an SVG string at `width`x`height` into a straight-alpha RGBA image (sRGB-encoded
/// color channels, as `tiny-skia`/`resvg` produce -- converted to linear before compositing).
pub fn rasterize_svg(svg: &str, width: u32, height: u32) -> anyhow::Result<RgbaImage> {
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_str(svg, &opt).map_err(|e| anyhow::anyhow!("parsing SVG: {e}"))?;

    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| anyhow::anyhow!("zero-sized watermark pixmap"))?;

    let tree_size = tree.size();
    let scale_x = width as f32 / tree_size.width();
    let scale_y = height as f32 / tree_size.height();
    let transform = tiny_skia::Transform::from_scale(scale_x, scale_y);

    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // tiny-skia's Pixmap stores premultiplied alpha; un-premultiply into a plain RGBA buffer so
    // downstream compositing always starts from the same straight-alpha representation whether
    // the source was an SVG or a PNG logo.
    let mut out = RgbaImage::new(width, height);
    for (dst, px) in out.pixels_mut().zip(pixmap.pixels()) {
        let a = px.alpha();
        if a == 0 {
            *dst = Rgba([0, 0, 0, 0]);
        } else {
            let unpremul = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
            *dst = Rgba([
                unpremul(px.red()),
                unpremul(px.green()),
                unpremul(px.blue()),
                a,
            ]);
        }
    }
    Ok(out)
}

/// Alpha-composites `overlay` (straight alpha, sRGB-encoded, e.g. from [`rasterize_svg`] or a
/// decoded PNG logo) onto `base` at `(x, y)`, in linear-premultiplied light. Sequential
/// reference implementation; see [`composite_parallel`] for the rayon-parallel version measured
/// against it.
pub fn composite_sequential(base: &mut RgbaImage, overlay: &RgbaImage, x: i64, y: i64) {
    let (ow, oh) = overlay.dimensions();
    for oy in 0..oh {
        for ox in 0..ow {
            composite_pixel(base, overlay, ox, oy, x, y);
        }
    }
}

/// Same blend as [`composite_sequential`], parallelized over overlay rows with `rayon`.
pub fn composite_parallel(base: &mut RgbaImage, overlay: &RgbaImage, x: i64, y: i64) {
    let (bw, bh) = base.dimensions();
    let (ow, oh) = overlay.dimensions();

    // Compute each destination row's blended pixels in parallel, then write back sequentially --
    // ImageBuffer's pixel storage isn't trivially splittable into disjoint mutable row slices
    // across arbitrary (x, y) offsets without extra bounds bookkeeping, and this keeps the
    // parallel/sequential code paths doing the exact same per-pixel math for a fair comparison.
    use rayon::prelude::*;
    type Row = (u32, Vec<(u32, Rgba<u8>)>);
    let rows: Vec<Row> = (0..oh)
        .into_par_iter()
        .filter_map(|oy| {
            let dst_y = y + oy as i64;
            if dst_y < 0 || dst_y as u32 >= bh {
                return None;
            }
            let mut row = Vec::with_capacity(ow as usize);
            for ox in 0..ow {
                let dst_x = x + ox as i64;
                if dst_x < 0 || dst_x as u32 >= bw {
                    continue;
                }
                let blended = blend(
                    base.get_pixel(dst_x as u32, dst_y as u32),
                    overlay.get_pixel(ox, oy),
                );
                row.push((dst_x as u32, blended));
            }
            Some((dst_y as u32, row))
        })
        .collect();

    for (dst_y, row) in rows {
        for (dst_x, color) in row {
            base.put_pixel(dst_x, dst_y, color);
        }
    }
}

fn composite_pixel(base: &mut RgbaImage, overlay: &RgbaImage, ox: u32, oy: u32, x: i64, y: i64) {
    let (bw, bh) = base.dimensions();
    let dst_x = x + ox as i64;
    let dst_y = y + oy as i64;
    if dst_x < 0 || dst_y < 0 || dst_x as u32 >= bw || dst_y as u32 >= bh {
        return;
    }
    let blended = blend(
        base.get_pixel(dst_x as u32, dst_y as u32),
        overlay.get_pixel(ox, oy),
    );
    base.put_pixel(dst_x as u32, dst_y as u32, blended);
}

/// "Over" compositing (Porter-Duff) in linear light, both operands straight (non-premultiplied)
/// alpha, sRGB-encoded color channels in and out.
fn blend(base: &Rgba<u8>, overlay: &Rgba<u8>) -> Rgba<u8> {
    let oa = overlay.0[3] as f32 / 255.0;
    if oa == 0.0 {
        return *base;
    }
    if oa == 1.0 {
        return *overlay;
    }
    let ba = base.0[3] as f32 / 255.0;
    let out_a = oa + ba * (1.0 - oa);

    let mut out = [0u8; 4];
    for (c, out_c) in out.iter_mut().enumerate().take(3) {
        let ol = srgb_eotf(overlay.0[c] as f32 / 255.0);
        let bl = srgb_eotf(base.0[c] as f32 / 255.0);
        // Straight-alpha "over": out = (over*oa + base*ba*(1-oa)) / out_a, then re-encode.
        let out_linear = if out_a > 0.0 {
            (ol * oa + bl * ba * (1.0 - oa)) / out_a
        } else {
            0.0
        };
        *out_c = (srgb_oetf(out_linear) * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    out[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
    Rgba(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_overlay(width: u32, height: u32, color: Rgba<u8>) -> RgbaImage {
        RgbaImage::from_pixel(width, height, color)
    }

    #[test]
    fn opaque_overlay_fully_replaces_base() {
        let mut base = RgbaImage::from_pixel(16, 16, Rgba([10, 20, 30, 255]));
        let overlay = solid_overlay(4, 4, Rgba([200, 0, 0, 255]));
        composite_sequential(&mut base, &overlay, 2, 2);
        assert_eq!(*base.get_pixel(3, 3), Rgba([200, 0, 0, 255]));
        // Outside the overlay's footprint, base is untouched.
        assert_eq!(*base.get_pixel(0, 0), Rgba([10, 20, 30, 255]));
    }

    #[test]
    fn zero_alpha_overlay_leaves_base_untouched() {
        let mut base = RgbaImage::from_pixel(8, 8, Rgba([10, 20, 30, 255]));
        let overlay = solid_overlay(8, 8, Rgba([200, 0, 0, 0]));
        composite_sequential(&mut base, &overlay, 0, 0);
        assert_eq!(*base.get_pixel(4, 4), Rgba([10, 20, 30, 255]));
    }

    #[test]
    fn half_alpha_overlay_is_brighter_in_linear_light_than_naive_gamma_average() {
        // The whole point of the linear-light decision rule: blending white-over-black at 50%
        // alpha in linear light is brighter than a naive average of the gamma-encoded bytes
        // (128 vs. 187), because sRGB's gamma curve compresses highlights.
        let mut base = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 255]));
        let overlay = solid_overlay(1, 1, Rgba([255, 255, 255, 128]));
        composite_sequential(&mut base, &overlay, 0, 0);
        let naive_gamma_average = 128u8;
        assert!(
            base.get_pixel(0, 0).0[0] > naive_gamma_average,
            "expected linear-light blend to exceed the naive gamma average, got {:?}",
            base.get_pixel(0, 0)
        );
    }

    #[test]
    fn sequential_and_parallel_composite_agree() {
        let overlay = RgbaImage::from_fn(32, 32, |x, y| {
            Rgba([(x * 8) as u8, (y * 8) as u8, 100, ((x + y) % 200) as u8])
        });
        let base_template = RgbaImage::from_fn(48, 48, |x, y| Rgba([x as u8, y as u8, 50, 255]));

        let mut seq = base_template.clone();
        composite_sequential(&mut seq, &overlay, 8, 8);

        let mut par = base_template.clone();
        composite_parallel(&mut par, &overlay, 8, 8);

        assert_eq!(
            seq, par,
            "sequential and parallel composite must produce identical output"
        );
    }

    #[test]
    fn composite_clips_at_base_boundary_without_panicking() {
        let mut base = RgbaImage::from_pixel(8, 8, Rgba([1, 2, 3, 255]));
        let overlay = solid_overlay(8, 8, Rgba([9, 9, 9, 255]));
        // Overlay partially off every edge -- must clip, not panic or wrap.
        composite_sequential(&mut base, &overlay, 4, 4);
        composite_sequential(&mut base, &overlay, -4, -4);
        assert_eq!(*base.get_pixel(0, 0), Rgba([9, 9, 9, 255]));
        assert_eq!(*base.get_pixel(7, 7), Rgba([9, 9, 9, 255]));
    }

    #[test]
    fn rasterize_svg_produces_requested_dimensions_and_nonzero_alpha() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
            <rect width="10" height="10" fill="#ff0000"/>
        </svg>"##;
        let rasterized = rasterize_svg(svg, 20, 20).unwrap();
        assert_eq!(rasterized.dimensions(), (20, 20));
        let center = rasterized.get_pixel(10, 10);
        assert_eq!(
            center.0[3], 255,
            "opaque rect should rasterize fully opaque"
        );
        assert!(
            center.0[0] > 200 && center.0[1] < 50,
            "expected red, got {center:?}"
        );
    }

    #[test]
    fn rasterized_svg_composites_onto_base() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
            <rect width="10" height="10" fill="#00ff00"/>
        </svg>"##;
        let overlay = rasterize_svg(svg, 10, 10).unwrap();
        let mut base = RgbaImage::from_pixel(20, 20, Rgba([0, 0, 0, 255]));
        composite_sequential(&mut base, &overlay, 5, 5);
        let px = base.get_pixel(10, 10);
        assert!(
            px.0[1] > 200 && px.0[0] < 50,
            "expected green watermark, got {px:?}"
        );
    }
}
