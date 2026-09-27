//! Bake-stage cost hypotheses, sourced from prior ADRs' own measured numbers rather than invented
//! here -- `sim.rs`'s hero-scenario simulation and `prefetch.rs`'s scheduling both need a duration
//! per bake stage, and the honest way to get one without a real end-to-end pipeline (which doesn't
//! exist yet -- that's #45's job) is to reuse what each stage's own research ticket already
//! measured in isolation. Every constant below cites its source; nothing here is fabricated.

use std::time::Duration;

/// ADR-0037 (`spikes/retina`): single-file LibRaw decode, Windows-native. The ADR reports a
/// 1.0-2.4s range; this uses the midpoint as a representative cost for the sim.
pub const DECODE_MID: Duration = Duration::from_millis(1700);

/// ADR-0040 (`spikes/rods`): SCUNet-PSNR full 6064x4040 frame, RTX 5080 CUDA EP. Real measured
/// wall time, not a hypothesis.
pub const DENOISE_FULL_RES: Duration = Duration::from_millis(50_900);

/// ADR-0048 (`spikes/siamese`): AI mask bake at preview resolution. **Hypothesis, not measured**
/// (ADR-0048's own Measured-results section: "TBD -- reference machine") -- this ADR's own #171
/// follow-up is what turns this into a real number. Used here so the sim can report a number at
/// all, clearly labelled as provisional in every place it's surfaced.
pub const MASK_BAKE_PREVIEW_RES_HYPOTHESIS: Duration = Duration::from_millis(1_000);

/// A 45MP (8280x5520, ADR-0029's Z8 raw-plane figure) RGBA16F intermediate: 4 channels * 2 bytes
/// (f16) * pixel count. ADR-0016 cites ~360MB for this; this computes it directly so the two never
/// drift out of sync.
pub fn full_res_rgba16f_bytes(width: u32, height: u32) -> u64 {
    width as u64 * height as u64 * 4 * 2
}

/// Screen-resolution tier (ADR-0029's T2: resized so the long edge is 3840px) at the same 4-channel
/// f16 layout Tapetum's VRAM/RAM tiers use for a baked frame.
pub fn screen_res_rgba16f_bytes(long_edge: u32, aspect: f32) -> u64 {
    let (w, h) = if aspect >= 1.0 {
        (long_edge, (long_edge as f32 / aspect).round() as u32)
    } else {
        ((long_edge as f32 * aspect).round() as u32, long_edge)
    };
    full_res_rgba16f_bytes(w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_res_z8_frame_matches_adr_0005s_cited_figure() {
        // ADR-0016 cites ~360MB for the 45.7MP RGBA16F intermediate; the Z8's own raw plane is
        // 8280x5520 (ADR-0029:47) = 45,705,600 pixels.
        let bytes = full_res_rgba16f_bytes(8280, 5520);
        let mb = bytes as f64 / (1024.0 * 1024.0);
        assert!((mb - 349.0).abs() < 10.0, "expected ~349MB, got {mb:.1}MB");
    }

    #[test]
    fn screen_res_is_much_smaller_than_full_res() {
        let full = full_res_rgba16f_bytes(8280, 5520);
        let screen = screen_res_rgba16f_bytes(3840, 8280.0 / 5520.0);
        assert!(screen < full / 3);
    }
}
