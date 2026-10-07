//! Real-NEF check for Nikon's embedded lens data (#410, ADR-0410). Gated on
//! `NICTI_TEST_REAL_NEF_DIR` (a directory of Z-series NEFs); without it the test prints "skipping"
//! and passes, like `nicti-cornea`'s `real_dng.rs`.
//!
//! It proves the locate -> decode -> model chain works on real files and dumps the decoded
//! coefficients so they can be compared by eye with `exiftool -G1 -a -s -*Distortion* -*Vignette*`
//! and with Adobe DNG Converter's `WarpRectilinear` for the same file -- the parity run that decides
//! whether `LensParams::nikon_profile` can become the default (ADR-0410).

use nicti_cornea::embedded::{SliceSource, Walker};
use nicti_iris::nikon::{parse, NikonEmbedded};
use nicti_iris::{LensCorrection, LensSource};

#[test]
fn real_nefs_decode_to_sane_models() {
    let Some(dir) = std::env::var_os("NICTI_TEST_REAL_NEF_DIR") else {
        eprintln!("skipping: NICTI_TEST_REAL_NEF_DIR is not set");
        return;
    };
    let mut seen = 0;
    let mut with_profile = 0;
    for entry in std::fs::read_dir(dir).expect("readable dir").flatten() {
        let path = entry.path();
        if !path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("nef"))
        {
            continue;
        }
        seen += 1;
        let data = std::fs::read(&path).expect("readable file");
        let Some(blob) = Walker::new(SliceSource::new(&data))
            .ok()
            .and_then(|mut w| w.find_nikon_lens_info().ok().flatten())
        else {
            eprintln!("{}: no 0xC7D5 blob", path.display());
            continue;
        };
        let info = parse(&blob).unwrap_or_else(|| panic!("{}: blob did not parse", path.display()));
        eprintln!(
            "{}: distortion {:?} vignette {:?}",
            path.display(),
            info.distortion,
            info.vignette
        );
        let model = NikonEmbedded
            .model(&LensSource {
                nikon_lens_info: Some(&blob),
                ..LensSource::default()
            })
            .unwrap_or_else(|| panic!("{}: no model from a parsed blob", path.display()));
        if let Some(w) = &model.warp {
            // A real lens profile is a small perturbation of the identity warp.
            let [k0, ..] = w.planes[0];
            assert!((k0 - 1.0).abs() < 0.1, "{}: k0 {k0}", path.display());
        }
        if let Some(v) = &model.vignette {
            let corner = v.gain(1.0);
            assert!(
                (1.0..4.0).contains(&corner),
                "{}: corner gain {corner}",
                path.display()
            );
        }
        with_profile += 1;
    }
    eprintln!("{with_profile}/{seen} NEFs carried a usable Nikon lens profile");
}
