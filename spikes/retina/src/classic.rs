//! `retina dump-classic`: for #40/ADR-0040's demosaic + noise-reduction spike. Runs LibRaw's
//! classic pipeline with WB applied and the caller's choice of demosaic algorithm plus NR knobs
//! (see shim.h's `retina_libraw_process_classic`), writing a linear 16-bit camera-RGB TIFF +
//! metadata JSON sidecar for `spikes/rods` to consume.
//!
//! Distinct from `dump-linear` (#38/calico's WB-free stand-in): this is the actual candidate
//! output #40 measures against LRC, not a hand-off format for a downstream color pipeline.
//!
//! Output per input file, named `<stem>.<quality>.fbdd<N>.wav<T>.classic.tiff` /
//! `.meta.json` so a sweep across settings doesn't clobber itself:
//! - `<stem>...classic.tiff`: 16-bit RGB TIFF, WB-applied, demosaiced, no color matrix / gamma.
//! - `<stem>...meta.json`: the same sidecar shape as `dump-linear`'s [`crate::linear::LinearMeta`]
//!   (`rods`'s reader is shared between the two).

use std::fs;
use std::path::{Path, PathBuf};

use image::{ImageBuffer, Rgb};

use crate::libraw_ffi::{DemosaicQuality, LibRawHandle};
use crate::linear::LinearMeta;

/// Writes `<stem>.<tag>.classic.tiff` + `.meta.json` into `out_dir` (created if missing).
pub fn dump_classic(
    path: &Path,
    out_dir: &Path,
    quality: DemosaicQuality,
    fbdd_noiserd: i32,
    wavelet_threshold: f32,
) -> anyhow::Result<()> {
    fs::create_dir_all(out_dir)?;
    let data = fs::read(path)?;

    let mut handle = LibRawHandle::new();
    handle
        .decode(&data)
        .map_err(|e| anyhow::anyhow!("decode {}: {e}", path.display()))?;

    // Same ordering caveat as dump_linear: capture pre-process metadata before dcraw_process()
    // can mutate imgdata.color.maximum via scale_colors().
    let meta = handle.metadata();

    handle
        .process_classic(quality, fbdd_noiserd, wavelet_threshold)
        .map_err(|e| anyhow::anyhow!("process_classic {}: {e}", path.display()))?;

    let linear = handle.linear_metadata();
    let image = handle.classic_image()?;

    let width = meta.iwidth as u32;
    let height = meta.iheight as u32;
    let expected_len = width as usize * height as usize * 4;
    anyhow::ensure!(
        image.len() == expected_len,
        "classic_image length {} != iwidth*iheight*4 ({expected_len}) for {}",
        image.len(),
        path.display()
    );

    let mut buf: ImageBuffer<Rgb<u16>, Vec<u16>> = ImageBuffer::new(width, height);
    for (i, px) in buf.pixels_mut().enumerate() {
        let base = i * 4;
        px.0 = [image[base], image[base + 1], image[base + 2]];
    }

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let tag = format!(
        "{quality:?}.fbdd{fbdd_noiserd}.wav{wavelet_threshold}",
        quality = quality
    )
    .to_lowercase();
    let tiff_path: PathBuf = out_dir.join(format!("{stem}.{tag}.classic.tiff"));
    let json_path: PathBuf = out_dir.join(format!("{stem}.{tag}.meta.json"));

    buf.save(&tiff_path)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", tiff_path.display()))?;

    let sidecar = LinearMeta {
        make: meta.make.clone(),
        model: meta.model.clone(),
        width,
        height,
        black: meta.black,
        maximum: meta.maximum,
        cam_mul: meta.cam_mul,
        pre_mul: linear.pre_mul,
        cam_xyz: linear.cam_xyz,
        cblack: linear.cblack,
    };
    fs::write(&json_path, serde_json::to_string_pretty(&sidecar)?)?;

    eprintln!(
        "wrote {} ({width}x{height}) and {}",
        tiff_path.display(),
        json_path.display()
    );
    Ok(())
}
