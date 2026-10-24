//! `.thumb.*` sidecar files: the on-disk home of an archived folder's T0 grid thumbnails
//! (#72, ADR-0072). A sidecar sits next to its RAW and carries the same bytes the catalog's
//! `preview` table holds for the active-edit tier, so an archive drive can be browsed without the
//! central cache.
//!
//! The name keeps the RAW's full file name (`DSC_1234.NEF` -> `DSC_1234.NEF.thumb.jpg`), unlike
//! the LRC-style `.xmp` sidecar: a `.NEF` and `.NRW` sharing a stem must not collide.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Preview;

/// Sidecar encoding. JPEG is a byte copy of the T0 (no re-encode); AVIF (#393, ADR-0143: speed 9
/// is the archival-tier candidate, ~2.7x smaller, slower decode) re-encodes on export. Lossy WebP
/// stays rejected (ADR-0143).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidecarCodec {
    #[default]
    Jpeg,
    Avif,
}

/// ravif quality / speed for AVIF sidecars (ADR-0143's archival-tier candidate range).
const AVIF_QUALITY: f32 = 80.0;
const AVIF_SPEED: u8 = 9;

impl SidecarCodec {
    /// Everything after the RAW's own file name.
    pub fn suffix(self) -> &'static str {
        match self {
            SidecarCodec::Jpeg => ".thumb.jpg",
            SidecarCodec::Avif => ".thumb.avif",
        }
    }

    fn other(self) -> SidecarCodec {
        match self {
            SidecarCodec::Jpeg => SidecarCodec::Avif,
            SidecarCodec::Avif => SidecarCodec::Jpeg,
        }
    }
}

/// `true` for a file name that is one of our sidecars, so importers never mistake it for an asset.
pub fn is_sidecar_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".thumb.jpg") || n.ends_with(".thumb.avif") || n.ends_with(".thumb-tmp")
}

pub fn sidecar_path(raw: &Path, codec: SidecarCodec) -> PathBuf {
    let mut name = raw.file_name().unwrap_or_default().to_os_string();
    name.push(codec.suffix());
    raw.with_file_name(name)
}

/// AVIF bytes for a T0 JPEG, or `None` when it must stay JPEG: a tagged ICC profile (ravif can't
/// carry one and this crate does no colour management, so AVIF would silently shift colour), an
/// EXIF rotation (the tag would be lost, #476) or bytes that don't decode.
fn encode_avif(jpeg: &[u8]) -> Option<Vec<u8>> {
    use image::{DynamicImage, ImageDecoder};
    let mut decoder = image::ImageReader::new(io::Cursor::new(jpeg))
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    if decoder.icc_profile().ok().flatten().is_some() {
        return None;
    }
    if decoder
        .orientation()
        .is_ok_and(|o| o != image::metadata::Orientation::NoTransforms)
    {
        return None;
    }
    let img = DynamicImage::from_decoder(decoder).ok()?.to_rgb8();
    let pixels: Vec<ravif::RGB8> = img
        .pixels()
        .map(|p| ravif::RGB8::new(p[0], p[1], p[2]))
        .collect();
    let enc = ravif::Encoder::new()
        .with_quality(AVIF_QUALITY)
        .with_speed(AVIF_SPEED)
        .with_num_threads(Some(1));
    let out = enc
        .encode_rgb(ravif::Img::new(
            pixels.as_slice(),
            img.width() as usize,
            img.height() as usize,
        ))
        .ok()?;
    Some(out.avif_file)
}

/// Writes `preview` next to `raw` atomically (temp file in the same folder, then rename) and
/// returns the codec actually written: an AVIF request falls back to JPEG per [`encode_avif`].
/// The other codec's sidecar, if any, is removed afterwards so one RAW never has two.
pub fn export(raw: &Path, preview: &Preview, codec: SidecarCodec) -> io::Result<SidecarCodec> {
    let avif = match codec {
        SidecarCodec::Avif => encode_avif(&preview.bytes),
        SidecarCodec::Jpeg => None,
    };
    let codec = if avif.is_some() {
        SidecarCodec::Avif
    } else {
        SidecarCodec::Jpeg
    };
    let bytes: &[u8] = avif.as_deref().unwrap_or(&preview.bytes);
    let path = sidecar_path(raw, codec);
    let tmp = path.with_file_name(format!(
        ".{}.thumb-tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let written = (|| {
        let mut f = fs::File::create(&tmp)?;
        io::Write::write_all(&mut f, bytes)?;
        // The rename path of a move doesn't re-read the sidecar, so make the bytes durable
        // before the name can be.
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    let _ = fs::remove_file(sidecar_path(raw, codec.other()));
    Ok(codec)
}

/// The sidecar next to `raw`: `preferred`'s file first, then the other codec's, so a folder
/// exported before the setting changed stays browsable. A missing, empty or malformed file reads
/// as `None` (never trust a truncated write).
pub fn read(raw: &Path, preferred: SidecarCodec) -> Option<Preview> {
    read_one(raw, preferred).or_else(|| read_one(raw, preferred.other()))
}

/// Which codec `raw`'s sidecar is in, if a valid one exists.
pub fn read_with_codec(raw: &Path, preferred: SidecarCodec) -> Option<(SidecarCodec, Preview)> {
    [preferred, preferred.other()]
        .into_iter()
        .find_map(|c| read_one(raw, c).map(|p| (c, p)))
}

fn read_one(raw: &Path, codec: SidecarCodec) -> Option<Preview> {
    let bytes = fs::read(sidecar_path(raw, codec)).ok()?;
    let (w, h) = match codec {
        SidecarCodec::Jpeg => {
            // A whole JPEG: SOI first, EOI last, and a frame header in between. A truncated
            // write must read as absent, since this is the gate for dropping the catalog's copy.
            if !bytes.starts_with(&[0xFF, 0xD8]) || !bytes.ends_with(&[0xFF, 0xD9]) {
                return None;
            }
            jpeg_dimensions(&bytes)?
        }
        SidecarCodec::Avif => avif_dimensions(&bytes)?,
    };
    Some(Preview {
        width: Some(w),
        height: Some(h),
        bytes,
    })
}

/// Pixel size of a whole AVIF file, from its container + sequence header (no pixel decode). A
/// truncated file fails the container parse. `avif-parse` asserts (panics) on some truncations
/// instead of returning `Err`, so the parse is caught: a bad file must read as absent, never
/// take the grid worker down.
pub fn avif_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    std::panic::catch_unwind(|| {
        let data = avif_parse::read_avif(&mut &b[..]).ok()?;
        let m = data.primary_item_metadata().ok()?;
        Some((m.max_frame_width.get(), m.max_frame_height.get()))
    })
    .ok()
    .flatten()
}

/// Decodes an AVIF sidecar to 8-bit RGB. `avif-parse`/`rav1d` may panic on a corrupt payload, so
/// the decode is caught: a bad file is `None`, never a dead worker thread. ravif's default bit depth
/// for 8-bit input is 10, which `avif-decode` returns as 16-bit.
pub fn decode_avif_rgb8(bytes: &[u8]) -> Option<image::RgbImage> {
    std::panic::catch_unwind(|| {
        let img = avif_decode::Decoder::from_avif(bytes)
            .and_then(|d| d.to_image())
            .ok()?;
        let (w, h, raw): (u32, u32, Vec<u8>) = match img {
            avif_decode::Image::Rgb8(i) => (
                i.width() as u32,
                i.height() as u32,
                i.buf().iter().flat_map(|p| [p.r, p.g, p.b]).collect(),
            ),
            avif_decode::Image::Rgb16(i) => (
                i.width() as u32,
                i.height() as u32,
                i.buf()
                    .iter()
                    .flat_map(|p| [(p.r >> 8) as u8, (p.g >> 8) as u8, (p.b >> 8) as u8])
                    .collect(),
            ),
            _ => return None,
        };
        image::RgbImage::from_raw(w, h, raw)
    })
    .ok()
    .flatten()
}

/// `true` when `avif` decodes and shows the same picture as the T0 `jpeg` it was encoded from:
/// same size, and a 16x16 block-average comparison within a small mean error. Size alone is no
/// proof (every T0 from one body shares it), and this also proves the AV1 payload decodes.
pub fn avif_matches_jpeg(avif: &[u8], jpeg: &[u8]) -> bool {
    let Some(a) = decode_avif_rgb8(avif) else {
        return false;
    };
    let Ok(j) = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg) else {
        return false;
    };
    let j = j.to_rgb8();
    if a.dimensions() != j.dimensions() || a.width() == 0 || a.height() == 0 {
        return false;
    }
    const N: u32 = 16;
    let (w, h) = a.dimensions();
    let mut total = 0u64;
    let mut cells = 0u64;
    for by in 0..N.min(h) {
        for bx in 0..N.min(w) {
            let (x0, x1) = (bx * w / N.min(w), (bx + 1) * w / N.min(w));
            let (y0, y1) = (by * h / N.min(h), (by + 1) * h / N.min(h));
            let mut sa = [0u64; 3];
            let mut sj = [0u64; 3];
            let mut n = 0u64;
            for y in y0..y1 {
                for x in x0..x1 {
                    for c in 0..3 {
                        sa[c] += a.get_pixel(x, y)[c] as u64;
                        sj[c] += j.get_pixel(x, y)[c] as u64;
                    }
                    n += 1;
                }
            }
            if let (Some(ma), Some(mj)) = (
                sa.map(|v| v.checked_div(n))
                    .into_iter()
                    .collect::<Option<Vec<_>>>(),
                sj.map(|v| v.checked_div(n))
                    .into_iter()
                    .collect::<Option<Vec<_>>>(),
            ) {
                total += ma.iter().zip(&mj).map(|(a, j)| a.abs_diff(*j)).sum::<u64>();
                cells += 3;
            }
        }
    }
    cells > 0 && total / cells <= MATCH_MEAN_ERR
}

/// Mean per-channel block error (0-255) under which an AVIF sidecar counts as its T0.
const MATCH_MEAN_ERR: u64 = 6;

/// Removes `raw`'s sidecar in every codec.
pub fn remove(raw: &Path) {
    let _ = fs::remove_file(sidecar_path(raw, SidecarCodec::Jpeg));
    let _ = fs::remove_file(sidecar_path(raw, SidecarCodec::Avif));
}

/// Pixel size from the first SOF marker, without decoding.
pub fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let marker = b[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            if i + 9 > b.len() {
                return None;
            }
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return Some((w, h));
        }
        if marker == 0xD8 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0, 0];
        v.extend([0xFF, 0xC0, 0x00, 0x0B, 8]);
        v.extend(h.to_be_bytes());
        v.extend(w.to_be_bytes());
        v.extend([1, 1, 0x11, 0]);
        v.extend([0xFF, 0xD9]);
        v
    }

    #[test]
    fn name_keeps_full_raw_name() {
        let p = sidecar_path(Path::new("/a/DSC_1.NEF"), SidecarCodec::Jpeg);
        assert_eq!(p, Path::new("/a/DSC_1.NEF.thumb.jpg"));
        assert_ne!(
            p,
            sidecar_path(Path::new("/a/DSC_1.NRW"), SidecarCodec::Jpeg)
        );
    }

    #[test]
    fn export_read_remove_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("DSC_1.NEF");
        let pv = Preview {
            width: Some(40),
            height: Some(30),
            bytes: jpeg(40, 30),
        };
        export(&raw, &pv, SidecarCodec::Jpeg).unwrap();
        let back = read(&raw, SidecarCodec::Jpeg).unwrap();
        assert_eq!(back.bytes, pv.bytes);
        assert_eq!((back.width, back.height), (Some(40), Some(30)));
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1, "no temp left");
        remove(&raw);
        assert!(read(&raw, SidecarCodec::Jpeg).is_none());
    }

    #[test]
    fn a_truncated_jpeg_reads_as_none() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("a.NEF");
        let full = jpeg(40, 30);
        fs::write(
            sidecar_path(&raw, SidecarCodec::Jpeg),
            &full[..full.len() - 2],
        )
        .unwrap();
        assert!(read(&raw, SidecarCodec::Jpeg).is_none(), "no EOI");
        fs::write(sidecar_path(&raw, SidecarCodec::Jpeg), &full[..6]).unwrap();
        assert!(
            read(&raw, SidecarCodec::Jpeg).is_none(),
            "cut before the frame header"
        );
    }

    #[test]
    fn truncated_or_non_jpeg_reads_as_none() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("a.NEF");
        fs::write(sidecar_path(&raw, SidecarCodec::Jpeg), b"").unwrap();
        assert!(read(&raw, SidecarCodec::Jpeg).is_none());
        fs::write(sidecar_path(&raw, SidecarCodec::Jpeg), b"nope").unwrap();
        assert!(read(&raw, SidecarCodec::Jpeg).is_none());
    }

    /// A real, decodable solid-colour JPEG (the hand-built `jpeg` above has no scan data).
    fn real_jpeg(w: u32, h: u32, icc: Option<&[u8]>) -> Vec<u8> {
        use image::ImageEncoder;
        let px: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i % 251) as u8, 90, 160])
            .collect();
        let mut out = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90);
        if let Some(p) = icc {
            enc.set_icc_profile(p.to_vec()).unwrap();
        }
        enc.write_image(&px, w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    fn real_preview(w: u32, h: u32, icc: Option<&[u8]>) -> Preview {
        Preview {
            width: Some(w),
            height: Some(h),
            bytes: real_jpeg(w, h, icc),
        }
    }

    #[test]
    fn avif_export_read_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("DSC_1.NEF");
        let pv = real_preview(64, 48, None);
        assert_eq!(
            export(&raw, &pv, SidecarCodec::Avif).unwrap(),
            SidecarCodec::Avif
        );
        let (codec, back) = read_with_codec(&raw, SidecarCodec::Avif).unwrap();
        assert_eq!(codec, SidecarCodec::Avif);
        assert_eq!((back.width, back.height), (Some(64), Some(48)));
        assert!(!sidecar_path(&raw, SidecarCodec::Jpeg).exists());
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1, "no temp left");
    }

    #[test]
    fn icc_tagged_t0_stays_jpeg_in_avif_mode() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("a.NEF");
        let pv = real_preview(32, 32, Some(b"not-a-real-profile"));
        assert_eq!(
            export(&raw, &pv, SidecarCodec::Avif).unwrap(),
            SidecarCodec::Jpeg
        );
        assert_eq!(read(&raw, SidecarCodec::Avif).unwrap().bytes, pv.bytes);
    }

    #[test]
    fn exif_rotated_t0_stays_jpeg_in_avif_mode() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("a.NEF");
        let mut pv = real_preview(32, 16, None);
        pv.bytes = crate::scruff::tag_orientation(pv.bytes, 6);
        assert_eq!(
            export(&raw, &pv, SidecarCodec::Avif).unwrap(),
            SidecarCodec::Jpeg
        );
        assert_eq!(read(&raw, SidecarCodec::Avif).unwrap().bytes, pv.bytes);
    }

    #[test]
    fn undecodable_t0_stays_jpeg_in_avif_mode() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("a.NEF");
        let pv = Preview {
            width: Some(40),
            height: Some(30),
            bytes: jpeg(40, 30),
        };
        assert_eq!(
            export(&raw, &pv, SidecarCodec::Avif).unwrap(),
            SidecarCodec::Jpeg
        );
    }

    #[test]
    fn truncated_or_garbage_avif_reads_as_none() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("a.NEF");
        export(&raw, &real_preview(64, 48, None), SidecarCodec::Avif).unwrap();
        let path = sidecar_path(&raw, SidecarCodec::Avif);
        let full = fs::read(&path).unwrap();
        for cut in [0, 8, 24, full.len() / 2, full.len() - 1] {
            fs::write(&path, &full[..cut]).unwrap();
            assert!(read(&raw, SidecarCodec::Avif).is_none(), "cut at {cut}");
        }
        fs::write(&path, b"nope nope nope nope").unwrap();
        assert!(read(&raw, SidecarCodec::Avif).is_none());
    }

    #[test]
    fn read_falls_back_to_the_other_codec_and_export_replaces_it() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("a.NEF");
        let pv = real_preview(64, 48, None);
        export(&raw, &pv, SidecarCodec::Jpeg).unwrap();
        let (c, _) = read_with_codec(&raw, SidecarCodec::Avif).unwrap();
        assert_eq!(c, SidecarCodec::Jpeg);
        export(&raw, &pv, SidecarCodec::Avif).unwrap();
        assert!(!sidecar_path(&raw, SidecarCodec::Jpeg).exists());
        let (c, _) = read_with_codec(&raw, SidecarCodec::Jpeg).unwrap();
        assert_eq!(c, SidecarCodec::Avif);
        remove(&raw);
        assert!(read(&raw, SidecarCodec::Jpeg).is_none());
    }

    #[test]
    #[ignore = "manual: NICTI_SIDECAR_JPEG=<a real T0-sized jpeg>, prints AVIF size/encode time"]
    fn measure_avif_sidecar() {
        let bytes = fs::read(std::env::var("NICTI_SIDECAR_JPEG").unwrap()).unwrap();
        let t = std::time::Instant::now();
        let avif = encode_avif(&bytes).expect("encodes");
        eprintln!(
            "jpeg {} B -> avif {} B ({:.2}x) in {} ms",
            bytes.len(),
            avif.len(),
            bytes.len() as f64 / avif.len() as f64,
            t.elapsed().as_millis()
        );
    }

    #[test]
    fn recognises_own_names() {
        assert!(is_sidecar_name("DSC_1.NEF.thumb.jpg"));
        assert!(is_sidecar_name("DSC_1.NEF.THUMB.JPG"));
        assert!(is_sidecar_name("DSC_1.NEF.thumb.avif"));
        assert!(!is_sidecar_name("DSC_1.jpg"));
    }
}
