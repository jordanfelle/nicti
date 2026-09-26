//! Decodes via rawler 0.8.0 (crates.io), for the Lossless Z8/D7500 cross-check against LibRaw
//! (#37's Correctness section) and the full-sweep "does rawler cleanly reject HE/HE*" check.

use rawler::decoders::RawDecodeParams;
use rawler::rawimage::RawImageData;
use rawler::rawsource::RawSource;

use crate::frame::RawFrame;

#[derive(Debug, thiserror::Error)]
pub enum RawlerError {
    #[error("rawler: {0}")]
    Rawler(String),
}

pub fn decode(data: &[u8]) -> Result<RawFrame, RawlerError> {
    let source = RawSource::new_from_slice(data);
    let decoder = rawler::get_decoder(&source).map_err(|e| RawlerError::Rawler(e.to_string()))?;
    let raw_image = decoder
        .raw_image(&source, &RawDecodeParams::default(), false)
        .map_err(|e| RawlerError::Rawler(e.to_string()))?;

    let cfa: &[u16] = match &raw_image.data {
        RawImageData::Integer(v) => v.as_slice(),
        RawImageData::Float(_) => {
            // Not expected for any ref-10k Nikon NEF (integer CFA in every bucket) -- surfaced as
            // an error rather than silently reinterpreting floats as u16, which `compare` would
            // otherwise report as a false CFA mismatch against LibRaw.
            return Err(RawlerError::Rawler(
                "decoded to RawImageData::Float, expected Integer (u16) -- not comparable to \
                 LibRaw's raw_image without a real float->u16 conversion, not implemented here"
                    .to_string(),
            ));
        }
    };

    Ok(RawFrame {
        make: raw_image.camera.clean_make.clone(),
        model: raw_image.camera.clean_model.clone(),
        // rawler doesn't surface the raw Nikon MakerNote tag value the same way LibRaw does --
        // it's already resolved the compression internally by the time `RawImage` exists. Left
        // at 0/"n/a (rawler)"; `compare` joins frames by manifest id, not this field, when one
        // side is a rawler decode.
        nef_compression: 0,
        compression_label: "n/a (rawler)".to_string(),
        raw_width: raw_image.width as u32,
        raw_height: raw_image.height as u32,
        top_margin: 0,
        left_margin: 0,
        // Not LibRaw's `filters` bitmask (rawler's `CFA` has no equivalent encoding) -- just the
        // distinct-color count, informational only. `compare` diffs `cfa_hash`, not this field.
        filters: raw_image.camera.cfa.unique_colors() as u32,
        // NOT `raw_image.cpp` -- a hostile review caught that field's actual meaning (rawler's
        // own doc comment: "number of components per pixel, 1 for bayer, 3 for RGB") is always 1
        // for every real file in this research, not the distinct-CFA-color count LibRaw's
        // `colors` field holds (typically 3 for Nikon's RGGB Bayer). Same value as `filters`
        // above -- rawler doesn't have two distinct concepts here the way LibRaw does.
        colors: raw_image.camera.cfa.unique_colors() as i32,
        black: raw_image
            .blacklevel
            .levels
            .first()
            .and_then(|r| r.n.checked_div(r.d))
            .unwrap_or(0),
        maximum: raw_image.whitelevel.0[0],
        // serde_json can serialize NaN (as `null`) but not deserialize it back into an `f32` --
        // `compare` round-trips these through JSON. rawler leaves an unused 4th wb_coeffs slot as
        // NaN rather than 0.0 for a 3-channel Bayer CFA, which is why this sanitizing exists at
        // all -- but a hostile review caught that an earlier fix only applied it to that 4th
        // slot, leaving indices 0-2 to the same failure mode on any real file where rawler
        // couldn't determine a WB coefficient. Sanitize all four uniformly.
        cam_mul: {
            let sanitize = |v: f32| if v.is_finite() { v } else { 0.0 };
            [
                sanitize(raw_image.wb_coeffs[0]),
                sanitize(raw_image.wb_coeffs[1]),
                sanitize(raw_image.wb_coeffs[2]),
                sanitize(raw_image.wb_coeffs.get(3).copied().unwrap_or(0.0)),
            ]
        },
        cfa_hash: crate::frame::hash_cfa(cfa),
        cfa_len: cfa.len(),
    })
}
