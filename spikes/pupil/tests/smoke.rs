//! End-to-end smoke test: writes a real 16-bit TIFF + JSON sidecar (the same on-disk shape
//! `retina dump-linear` produces) to a tempdir, then runs the real `input::load` ->
//! `render::default_render_luminance` -> `Histogram` -> `heuristic::estimate` pipeline against it
//! and checks the result is directionally sane and within PV2012's documented ranges.
//!
//! This substitutes for a smoke run against a real NEF: the prebuilt `retina.exe` available in
//! this sandbox (`/mnt/h/NictiBench-subset/retina.exe`, built 2026-09-26) predates the
//! `dump-linear` subcommand ADR-0038 added, and `spikes/retina`'s own LibRaw submodule isn't
//! initialized here to rebuild it -- see ADR-0099's Measured results section.

use image::{ImageBuffer, Rgb};
use pupil::{heuristic, histogram::Histogram, input, render};

fn write_fixture(dir: &std::path::Path, stem: &str, pixels: &ImageBuffer<Rgb<u16>, Vec<u16>>) {
    let tiff_path = dir.join(format!("{stem}.tiff"));
    pixels.save(&tiff_path).expect("writing fixture TIFF");

    let meta = serde_json::json!({
        "make": "NIKON CORPORATION",
        "model": "NIKON Z 8",
        "width": pixels.width(),
        "height": pixels.height(),
        "black": 0,
        "maximum": 65535,
        "cam_mul": [2.1, 1.0, 1.6, 0.0],
        "pre_mul": [2.1, 1.0, 1.6, 0.0],
        "cam_xyz": [0.6, 0.2, 0.1, 0.2, 0.8, 0.05, 0.05, 0.1, 0.9, 0.0, 0.0, 0.0],
        "cblack": [0, 0, 0, 0],
    });
    std::fs::write(
        dir.join(format!("{stem}.json")),
        serde_json::to_vec_pretty(&meta).unwrap(),
    )
    .expect("writing fixture JSON sidecar");
}

#[test]
fn dark_synthetic_frame_yields_positive_exposure_within_range() {
    let dir = tempfile::tempdir().unwrap();
    let (w, h) = (32, 24);
    let pixels = ImageBuffer::from_fn(w, h, |_, _| Rgb([3_000u16, 2_500, 2_200]));
    write_fixture(dir.path(), "dark", &pixels);

    let linear = input::load(&dir.path().join("dark.tiff"), &dir.path().join("dark.json"))
        .expect("loading fixture");
    let samples = render::default_render_luminance(&linear, 1);
    assert!(!samples.is_empty());
    let hist = Histogram::from_samples(samples);
    let sliders = heuristic::estimate(&hist);

    assert!(
        sliders.exposure2012 > 0.0,
        "a dark frame should get positive exposure, got {sliders:?}"
    );
    assert!((-5.0..=5.0).contains(&sliders.exposure2012));
    for v in [
        sliders.contrast2012,
        sliders.highlights2012,
        sliders.shadows2012,
        sliders.whites2012,
        sliders.blacks2012,
    ] {
        assert!(
            (-100.0..=100.0).contains(&v),
            "slider out of documented range: {v}"
        );
    }

    // Round-trips through JSON the way the `pupil auto` CLI subcommand prints it (allowing for
    // f64 string round-trip rounding in the last bit).
    let json = serde_json::to_string(&sliders).unwrap();
    let parsed: pupil::sliders::Sliders = serde_json::from_str(&json).unwrap();
    for (p, s) in parsed.as_array().iter().zip(sliders.as_array().iter()) {
        assert!((p - s).abs() < 1e-9, "JSON round-trip drifted: {p} vs {s}");
    }
}

#[test]
fn frame_with_a_blown_corner_yields_negative_highlights() {
    let dir = tempfile::tempdir().unwrap();
    let (w, h) = (32, 24);
    let mut pixels = ImageBuffer::from_fn(w, h, |_, _| Rgb([20_000u16, 18_000, 16_000]));
    for y in 0..8 {
        for x in 0..8 {
            pixels.put_pixel(x, y, Rgb([65_000, 65_000, 65_000]));
        }
    }
    write_fixture(dir.path(), "blown", &pixels);

    let linear = input::load(
        &dir.path().join("blown.tiff"),
        &dir.path().join("blown.json"),
    )
    .expect("loading fixture");
    let samples = render::default_render_luminance(&linear, 1);
    let hist = Histogram::from_samples(samples);
    let sliders = heuristic::estimate(&hist);

    assert!(
        sliders.highlights2012 < 0.0,
        "a partially blown frame should get negative highlights, got {sliders:?}"
    );
}
