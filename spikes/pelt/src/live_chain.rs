//! Live-chain compute kernel shared by every `pelt-*` viewport: WGSL source (copied from
//! `spikes/glint/shaders/live_chain.wgsl` -- spikes don't build on spikes, see `CLAUDE.md`'s
//! package-map note) plus a CPU reference and the fixed-size synthetic input frame the viewport
//! interaction (slider drag + pan) runs against. This spike measures UI/compositor input latency,
//! not GPU compute throughput -- `spikes/glint`/ADR-0005 already measured the kernel itself; this
//! crate exists only to put *some* real wgpu compute work behind each toolkit's custom-viewport
//! embedding, so the frame-interval numbers reflect an actual paint + present cycle, not an empty
//! quad.

use crate::config::{VIEWPORT_HEIGHT, VIEWPORT_WIDTH};

pub const LIVE_CHAIN_WGSL: &str = include_str!("../shaders/live_chain.wgsl");

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct LiveChainParams {
    pub wb_gain: [f32; 3],
    pub exposure_stops: f32,
    pub vibrance: f32,
    pub _pad: [f32; 3],
}

impl Default for LiveChainParams {
    fn default() -> Self {
        Self {
            wb_gain: [1.0, 1.0, 1.0],
            exposure_stops: 0.0,
            vibrance: 0.0,
            _pad: [0.0; 3],
        }
    }
}

/// Plain-`f32` reference, mirroring `spikes/glint/src/cpu_reference.rs::live_chain_pixel` exactly
/// -- kept only so a `pelt-*` binary's manual smoke test can sanity-check its GPU output against
/// something, not re-measured for correctness the way glint's own test suite already did for
/// ADR-0005.
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

fn tone_curve(x: f32) -> f32 {
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
    let weight = vibrance * (1.0 - sat);
    let avg = (rgb[0] + rgb[1] + rgb[2]) / 3.0;
    [
        (avg + (rgb[0] - avg) * (1.0 + weight)).clamp(0.0, 1.0),
        (avg + (rgb[1] - avg) * (1.0 + weight)).clamp(0.0, 1.0),
        (avg + (rgb[2] - avg) * (1.0 + weight)).clamp(0.0, 1.0),
    ]
}

/// Synthetic RGBA `[f32; 4]` viewport frame at `VIEWPORT_WIDTH`x`VIEWPORT_HEIGHT`, the input to
/// each `pelt-*` binary's live-chain compute pass. Deterministic, no real image content needed.
pub fn generate_viewport_frame() -> Vec<[f32; 4]> {
    let w = VIEWPORT_WIDTH as usize;
    let h = VIEWPORT_HEIGHT as usize;
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let u = x as f32 / w as f32;
            let v = y as f32 / h as f32;
            out.push([u, v, (u * v).fract(), 1.0]);
        }
    }
    out
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
    fn viewport_frame_has_expected_pixel_count() {
        let frame = generate_viewport_frame();
        assert_eq!(
            frame.len(),
            VIEWPORT_WIDTH as usize * VIEWPORT_HEIGHT as usize
        );
    }

    #[test]
    fn default_params_are_identity_ish() {
        let out = live_chain_pixel([0.5, 0.5, 0.5], [1.0, 1.0, 1.0], 0.0, 0.0);
        // Not a literal identity (the tone curve isn't linear), but should stay in range and
        // move all channels identically for a gray input with no vibrance.
        assert_eq!(out[0], out[1]);
        assert_eq!(out[1], out[2]);
    }
}
