//! Cheap JPEG-header inspection: SOF dimensions/subsampling and a DQT-based quality estimate.
//! Deliberately does not decode pixels -- that's `zune_jpeg`'s job in `decode.rs`. This only
//! walks markers, so it's safe to run over every embedded JPEG in the inventory pass.

#[derive(Debug, Clone, Copy, Default)]
pub struct JpegHeaderInfo {
    pub width: Option<u16>,
    pub height: Option<u16>,
    /// e.g. "4:2:0", "4:2:2", "4:4:4" -- derived from the luma component's sampling factors.
    pub subsampling: Option<&'static str>,
    /// IJG quality-scale estimate (1-100) from the luma DQT table, when present.
    pub quality_estimate: Option<u8>,
    pub has_soi: bool,
    pub has_eoi: bool,
}

/// The standard IJG luma quantization table at quality 50 -- the baseline every other quality
/// level's table is derived from by linear scaling. Used to back out an approximate quality from
/// an arbitrary DQT table (same method `libjpeg`'s `jpeg_quality_scaling` inverts).
#[rustfmt::skip]
const STD_LUMA_QT50: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61,
    12, 12, 14, 19, 26, 58, 60, 55,
    14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62,
    18, 22, 37, 56, 68, 109, 103, 77,
    24, 35, 55, 64, 81, 104, 113, 92,
    49, 64, 78, 87, 103, 121, 120, 101,
    72, 92, 95, 98, 112, 100, 103, 99,
];

fn estimate_quality(table: &[u16; 64]) -> u8 {
    // Average scaling factor across all 64 coefficients vs. the QT50 baseline, then inverted via
    // the same piecewise-linear relationship libjpeg uses to build tables from a quality scalar.
    let mut sum_scale = 0f64;
    let mut n = 0f64;
    for i in 0..64 {
        if STD_LUMA_QT50[i] == 0 {
            continue;
        }
        sum_scale += table[i] as f64 / STD_LUMA_QT50[i] as f64;
        n += 1.0;
    }
    if n == 0.0 {
        return 0;
    }
    let scale = (sum_scale / n * 100.0).round();
    let quality = if scale <= 100.0 {
        (200.0 - scale) / 2.0
    } else {
        5000.0 / scale
    };
    quality.round().clamp(1.0, 100.0) as u8
}

pub fn inspect(jpeg: &[u8]) -> JpegHeaderInfo {
    let mut info = JpegHeaderInfo::default();
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return info;
    }
    info.has_soi = true;
    info.has_eoi = jpeg.len() >= 2 && jpeg[jpeg.len() - 2] == 0xFF && jpeg[jpeg.len() - 1] == 0xD9;

    let mut luma_qt_id: Option<u8> = None;
    let mut qtables: [Option<[u16; 64]>; 4] = [None; 4];

    let mut pos = 2usize;
    while pos + 4 <= jpeg.len() {
        if jpeg[pos] != 0xFF {
            pos += 1;
            continue;
        }
        let marker = jpeg[pos + 1];
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            pos += 2;
            continue;
        }
        if marker == 0xD9 {
            break;
        }
        if pos + 4 > jpeg.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]) as usize;
        if seg_len < 2 || pos + 2 + seg_len > jpeg.len() {
            break;
        }
        let payload = &jpeg[pos + 4..pos + 2 + seg_len];

        match marker {
            0xC0..=0xC3 => {
                // SOF0/1/2/3: precision(1) height(2) width(2) num_components(1) then per-component.
                if payload.len() >= 6 {
                    info.height = Some(u16::from_be_bytes([payload[1], payload[2]]));
                    info.width = Some(u16::from_be_bytes([payload[3], payload[4]]));
                    let n_comp = payload[5] as usize;
                    if n_comp >= 1 && payload.len() >= 6 + n_comp * 3 {
                        let luma = &payload[6..9]; // [component_id, sampling_factors, qt_selector]
                        luma_qt_id = Some(luma[2] & 0x0F);
                        let h = (luma[1] >> 4) & 0x0F;
                        let v = luma[1] & 0x0F;
                        info.subsampling = match (h, v) {
                            (2, 2) => Some("4:2:0"),
                            (2, 1) => Some("4:2:2"),
                            (1, 1) => Some("4:4:4"),
                            (1, 2) => Some("4:4:0"),
                            _ => None,
                        };
                    }
                }
            }
            0xDB => {
                // DQT: one or more (precision/id byte + 64 entries) blocks per segment.
                let mut p = 0usize;
                while p < payload.len() {
                    let pq_tq = payload[p];
                    let precision = pq_tq >> 4;
                    let id = (pq_tq & 0x0F) as usize;
                    p += 1;
                    if id >= 4 {
                        break;
                    }
                    let mut table = [0u16; 64];
                    if precision == 0 {
                        if p + 64 > payload.len() {
                            break;
                        }
                        for i in 0..64 {
                            table[i] = payload[p + i] as u16;
                        }
                        p += 64;
                    } else {
                        if p + 128 > payload.len() {
                            break;
                        }
                        for i in 0..64 {
                            table[i] =
                                u16::from_be_bytes([payload[p + i * 2], payload[p + i * 2 + 1]]);
                        }
                        p += 128;
                    }
                    qtables[id] = Some(table);
                }
            }
            0xDA => break, // SOS: entropy-coded data follows, nothing after here matters to us.
            _ => {}
        }
        pos += 2 + seg_len;
    }

    if let Some(id) = luma_qt_id {
        if let Some(Some(table)) = qtables.get(id as usize) {
            info.quality_estimate = Some(estimate_quality(table));
        }
    }

    info
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, marker];
        v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn parses_sof0_dimensions_and_subsampling_420() {
        let mut jpeg = vec![0xFF, 0xD8];
        // DQT id 0, precision 0, all-16s table (arbitrary but valid).
        let mut dqt_payload = vec![0x00];
        dqt_payload.extend_from_slice(&[16u8; 64]);
        jpeg.extend(segment(0xDB, &dqt_payload));
        // SOF0: precision=8, height=100, width=200, 1 component: id=1, sampling=0x22 (4:2:0), qt=0
        let sof = [8, 0, 100, 0, 200, 1, 1, 0x22, 0];
        jpeg.extend(segment(0xC0, &sof));
        jpeg.extend_from_slice(&[0xFF, 0xD9]);

        let info = inspect(&jpeg);
        assert!(info.has_soi);
        assert!(info.has_eoi);
        assert_eq!(info.width, Some(200));
        assert_eq!(info.height, Some(100));
        assert_eq!(info.subsampling, Some("4:2:0"));
        assert!(info.quality_estimate.is_some());
    }

    #[test]
    fn rejects_non_jpeg_input() {
        let info = inspect(&[0, 1, 2, 3]);
        assert!(!info.has_soi);
        assert_eq!(info.width, None);
    }

    #[test]
    fn quality_estimate_high_for_low_scaling_table() {
        // Table equal to the QT50 baseline itself should estimate close to 50.
        let mut jpeg = vec![0xFF, 0xD8];
        let mut dqt_payload = vec![0x00];
        for v in STD_LUMA_QT50 {
            dqt_payload.push(v as u8);
        }
        jpeg.extend(segment(0xDB, &dqt_payload));
        let sof = [8, 0, 10, 0, 10, 1, 1, 0x11, 0];
        jpeg.extend(segment(0xC0, &sof));
        jpeg.extend_from_slice(&[0xFF, 0xD9]);

        let info = inspect(&jpeg);
        let q = info.quality_estimate.expect("quality estimate");
        assert!((45..=55).contains(&q), "expected ~50, got {q}");
    }
}
