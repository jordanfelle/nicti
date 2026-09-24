//! Plain-`f32` CPU implementations of the kernels under test. Every GPU backend (wgpu-Vulkan,
//! wgpu-Dx12, CUDA) is checked against this within f16 tolerance in `tests/correctness.rs` — a
//! shader that merely runs is not proof it's correct.

/// Fused "live" stage chain: white balance (per-channel gain) -> exposure (stops) -> a small
/// tone-curve LUT (piecewise-linear) -> vibrance (saturation boost weighted by existing
/// saturation, skin-tone-safe midpoint). Mirrors the stages Tapetum (#44) keeps live in the
/// shader rather than caching, applied per-pixel over RGB (ignoring alpha).
pub fn live_chain_pixel(
    rgb: [f32; 3],
    wb_gain: [f32; 3],
    exposure_stops: f32,
    vibrance: f32,
) -> [f32; 3] {
    let exposure_mul = 2f32.powf(exposure_stops);
    let mut c = [
        rgb[0] * wb_gain[0] * exposure_mul,
        rgb[1] * wb_gain[1] * exposure_mul,
        rgb[2] * wb_gain[2] * exposure_mul,
    ];
    for v in &mut c {
        *v = tone_curve(*v);
    }
    apply_vibrance(c, vibrance)
}

/// 5-point piecewise-linear tone curve (shadows lifted, highlights rolled off), evaluated at
/// fixed control points x = 0, 0.25, 0.5, 0.75, 1.0. Representative of a LUT-based tone stage,
/// not a claim about Nicti's eventual real curve shape.
pub fn tone_curve(x: f32) -> f32 {
    const XS: [f32; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];
    const YS: [f32; 5] = [0.02, 0.22, 0.5, 0.80, 0.98];
    let x = x.clamp(0.0, 1.0);
    for i in 0..XS.len() - 1 {
        if x <= XS[i + 1] || i == XS.len() - 2 {
            let t = (x - XS[i]) / (XS[i + 1] - XS[i]);
            return YS[i] + t * (YS[i + 1] - YS[i]);
        }
    }
    unreachable!("x clamped to [0, 1], loop covers full range")
}

fn apply_vibrance(rgb: [f32; 3], vibrance: f32) -> [f32; 3] {
    let max = rgb[0].max(rgb[1]).max(rgb[2]);
    let min = rgb[0].min(rgb[1]).min(rgb[2]);
    let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
    // Weight vibrance's effect down as sat approaches 1 so already-saturated (often skin-tone)
    // pixels move less than desaturated ones for the same vibrance value.
    let weight = vibrance * (1.0 - sat);
    let avg = (rgb[0] + rgb[1] + rgb[2]) / 3.0;
    [
        (avg + (rgb[0] - avg) * (1.0 + weight)).clamp(0.0, 1.0),
        (avg + (rgb[1] - avg) * (1.0 + weight)).clamp(0.0, 1.0),
        (avg + (rgb[2] - avg) * (1.0 + weight)).clamp(0.0, 1.0),
    ]
}

/// Feathered linear blend of two overlapping tiles along their shared seam, `t` in `[0, 1]`
/// where 0 is fully tile `a` and 1 is fully tile `b`. Stand-in for the render-side half of tiled
/// AI inference (the AI model itself runs in ONNX Runtime, per ADR-0004 §3; this is the seam
/// reconstruction that happens back on the render side).
pub fn tile_blend_pixel(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_curve_endpoints() {
        assert!((tone_curve(0.0) - 0.02).abs() < 1e-6);
        assert!((tone_curve(1.0) - 0.98).abs() < 1e-6);
    }

    #[test]
    fn tone_curve_midpoint() {
        assert!((tone_curve(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn tone_curve_clamps_out_of_range() {
        assert_eq!(tone_curve(-1.0), tone_curve(0.0));
        assert_eq!(tone_curve(2.0), tone_curve(1.0));
    }

    #[test]
    fn vibrance_zero_is_identity() {
        let rgb = [0.6, 0.3, 0.2];
        assert_eq!(apply_vibrance(rgb, 0.0), rgb);
    }

    #[test]
    fn vibrance_desaturates_negative() {
        let rgb = [0.8, 0.2, 0.2];
        let out = apply_vibrance(rgb, -1.0);
        let sat_in = (0.8f32 - 0.2) / 0.8;
        let max_out = out[0].max(out[1]).max(out[2]);
        let min_out = out[0].min(out[1]).min(out[2]);
        let sat_out = if max_out > 0.0 {
            (max_out - min_out) / max_out
        } else {
            0.0
        };
        assert!(sat_out < sat_in);
    }

    #[test]
    fn tile_blend_endpoints() {
        let a = [1.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0];
        assert_eq!(tile_blend_pixel(a, b, 0.0), a);
        assert_eq!(tile_blend_pixel(a, b, 1.0), b);
    }

    #[test]
    fn tile_blend_midpoint() {
        let a = [1.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0];
        let mid = tile_blend_pixel(a, b, 0.5);
        assert!((mid[0] - 0.5).abs() < 1e-6);
        assert!((mid[1] - 0.5).abs() < 1e-6);
    }
}
