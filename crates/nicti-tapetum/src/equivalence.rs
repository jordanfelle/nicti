//! CPU-vs-GPU equivalence table (#538, item 1 of #433).
//!
//! Two checks:
//!
//! 1. [`the_live_suffix_matches_its_cpu_twin_for_every_edit`]: a table of edit closures over
//!    [`LiveParams`] (each slider of each CPU-twinned stage, both directions, plus combinations and
//!    a white-balance variant). Every case renders one fixed test image through the real GPU live
//!    suffix and through the `color::` CPU twins, compares in linear working space (not sRGB-encoded; clamped to
//!    0..1, x255) and bounds mean |d| < 0.5 LSB and max |d| <= 3 LSB.
//! 2. [`every_render_stage_is_classified`]: every id in the production spine (baked, live,
//!    geometry) must appear in [`COVERAGE`] as parity-tested (naming a test that must exist in the
//!    named source), covered by the table above, exempt (with a reason) or a recorded gap. A new
//!    stage fails here until someone decides which, and the gap count can only go down.
//!
//! The table fails loudly instead of skipping when `NICTI_REQUIRE_GPU` is set and there is no
//! adapter (the required CI runner sets it); lavapipe is enough for the headless run.

use crate::coat::{ExposureParams, HslBand, ToneCurveParams, ToneParams, VibranceParams, WbParams};
use crate::color;
use crate::frame::{Extent, FrameTexture};
use crate::renderer::LiveExec;
use crate::spine::{BAKED_IDS, GEOMETRY_IDS, LIVE_IDS};
use crate::stages::{
    LiveParams, LiveSuffixKernel, CALIBRATION, COLOR_GRADE, CROP, DECODE, DEFRINGE, DEMOSAIC,
    DENOISE, EFFECTS, EXPOSURE, HEAL, HSL, LENS, MASKS, NOISE_REDUCTION, POINT_COLOR, POINT_CURVE,
    PRESENCE, SHARPEN, TONE, TONE_CURVE, VIBRANCE, WB, WORKING_SPACE,
};
use crate::test_util::{read_frame, required_test_gpu, upload_frame};

/// Mean |d| bound, in 8-bit LSB.
const MEAN_LSB: f32 = 0.5;
/// Max |d| bound, in 8-bit LSB.
const MAX_LSB: f32 = 3.0;

/// Cases known to exceed [`MAX_LSB`] on a single pixel, with the bound they are held to instead.
/// `tone shadows -1`: at full strength one strongly out-of-gamut pixel (input
/// [0.52, 0.76, 0.76] -> output [1.0, ~0, 0.95]) differs by 3.6 LSB on the blue channel (GPU
/// 0.953, CPU 0.939); the mean over the image is 0.03 LSB. Tracked in #701; delete the
/// entry when it is fixed, and the test then holds the case to [`MAX_LSB`] again.
const KNOWN_DIVERGENCES: &[(&str, f32)] = &[("tone shadows -1 [camera]", 4.5)];

const W: u32 = 32;
const H: u32 = 16;

/// A fixed, deterministic test image: a hue sweep across x, lightness down y, with the first row
/// forced to greys and the last column to near-black/near-white extremes, so neutral axis, shadows
/// and highlights are all exercised.
fn test_image() -> Vec<[f32; 4]> {
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        for x in 0..W {
            let (fx, fy) = (x as f32 / (W - 1) as f32, y as f32 / (H - 1) as f32);
            let rgb = if y == 0 {
                let g = fx; // a grey ramp
                [g, g, g]
            } else if x == W - 1 {
                if y % 2 == 0 {
                    [0.01, 0.015, 0.01]
                } else {
                    [0.95, 0.9, 0.92]
                }
            } else {
                hsv_to_rgb(fx * 360.0 * 0.97, 0.15 + 0.8 * (1.0 - fy), 0.08 + 0.85 * fy)
            };
            out.push([rgb[0], rgb[1], rgb[2], 1.0]);
        }
    }
    out
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let c = v * s;
    let hp = (h / 60.0).rem_euclid(6.0);
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [r + m, g + m, b + m]
}

/// The CPU twin of the live suffix for the stages the table covers, in shader order: working-space
/// matrix, exposure, tone, tone curve, vibrance, HSL.
fn cpu_chain(px: [f32; 3], p: &LiveParams) -> [f32; 3] {
    let mut rgb = color::mat3_apply(p.working_space_matrix, px);
    let mult = color::exposure_multiplier(p.exposure.stops);
    rgb = rgb.map(|c| c * mult);
    rgb = color::apply_tone(rgb, &p.tone);
    let lut = color::build_tone_curve_lut(&p.tone_curve);
    rgb = color::apply_tone_curve(rgb, &lut);
    rgb = color::apply_vibrance(rgb, p.vibrance.amount);
    color::apply_hsl(rgb, &p.hsl)
}

/// The camera -> ProPhoto matrix the GPU and CPU both start from.
fn base_matrix(wb: &WbParams) -> color::Mat3 {
    let cam_mul = [1.8, 1.0, 1.3, 1.0];
    let cam_xyz = [
        0.55, 0.2, 0.1, 0.2, 0.7, 0.15, 0.05, 0.1, 0.85, 0.0, 0.0, 0.0,
    ];
    color::camera_to_working_space_matrix(cam_mul, &cam_xyz, wb)
}

type Edit = Box<dyn Fn(&mut LiveParams)>;

/// The table: (name, edit). Built from sweeps so a new slider direction is one line.
fn cases() -> Vec<(String, Edit)> {
    let mut v: Vec<(String, Edit)> = Vec::new();
    let mut add = |name: String, f: Edit| v.push((name, f));

    add("identity".into(), Box::new(|_| {}));
    for stops in [-2.0f32, -0.5, 0.7, 2.0] {
        add(
            format!("exposure {stops:+}"),
            Box::new(move |p| p.exposure = ExposureParams { stops }),
        );
    }
    for amount in [-0.8f32, 0.4, 1.0] {
        add(
            format!("vibrance {amount:+}"),
            Box::new(move |p| p.vibrance = VibranceParams { amount }),
        );
    }
    for (sign, mag) in [-1.0f32, 1.0]
        .into_iter()
        .flat_map(|s| [(s, 0.3f32), (s, 1.0f32)])
    {
        for (field, set) in [
            (
                "contrast",
                (|t: &mut ToneParams, x| t.contrast = x) as fn(&mut ToneParams, f32),
            ),
            ("highlights", |t, x| t.highlights = x),
            ("shadows", |t, x| t.shadows = x),
            ("whites", |t, x| t.whites = x),
            ("blacks", |t, x| t.blacks = x),
        ] {
            add(
                format!("tone {field} {:+}", mag * sign),
                Box::new(move |p| set(&mut p.tone, mag * sign)),
            );
        }
        for (field, set) in [
            (
                "shadows",
                (|t: &mut ToneCurveParams, x| t.shadows = x) as fn(&mut ToneCurveParams, f32),
            ),
            ("darks", |t, x| t.darks = x),
            ("lights", |t, x| t.lights = x),
            ("highlights", |t, x| t.highlights = x),
        ] {
            // `build_tone_curve_lut` clamps its control points to 0..1, so pushing the shadows down
            // or the highlights up changes nothing; those directions would be silent no-ops.
            if (field == "shadows" && sign < 0.0) || (field == "highlights" && sign > 0.0) {
                continue;
            }
            add(
                format!("tone curve {field} {:+}", mag * sign),
                Box::new(move |p| set(&mut p.tone_curve, mag * sign)),
            );
        }
        for band in 0..8usize {
            for (ch, set) in [
                (
                    "hue",
                    (|b: &mut HslBand, x| b.hue = x) as fn(&mut HslBand, f32),
                ),
                ("saturation", |b, x| b.saturation = x),
                ("luminance", |b, x| b.luminance = x),
            ] {
                add(
                    format!("hsl band {band} {ch} {:+}", mag * sign),
                    Box::new(move |p| set(&mut p.hsl.bands[band], mag * sign)),
                );
            }
        }
    }
    for (name, wb) in [
        (
            "wb warm",
            WbParams {
                temp_k: Some(3200.0),
                tint: 0.0,
            },
        ),
        (
            "wb cool",
            WbParams {
                temp_k: Some(9000.0),
                tint: 0.0,
            },
        ),
        (
            "wb green tint",
            WbParams {
                temp_k: None,
                tint: 60.0,
            },
        ),
        (
            "wb magenta tint",
            WbParams {
                temp_k: None,
                tint: -60.0,
            },
        ),
    ] {
        add(
            name.to_string(),
            Box::new(move |p| p.working_space_matrix = base_matrix(&wb)),
        );
    }
    add(
        "kitchen sink".into(),
        Box::new(|p| {
            p.exposure = ExposureParams { stops: 0.5 };
            p.tone = ToneParams {
                contrast: 0.3,
                highlights: -0.4,
                shadows: 0.35,
                whites: 0.1,
                blacks: -0.1,
            };
            p.tone_curve = ToneCurveParams {
                shadows: 0.2,
                darks: -0.1,
                lights: 0.15,
                highlights: -0.05,
            };
            p.vibrance = VibranceParams { amount: 0.5 };
            p.hsl.bands[0] = HslBand {
                hue: 0.3,
                saturation: -0.4,
                luminance: 0.2,
            };
            p.hsl.bands[5] = HslBand {
                hue: -0.2,
                saturation: 0.5,
                luminance: -0.3,
            };
        }),
    );
    v
}

/// Error of `gpu` against `cpu`, linear values clamped to 0..1 and scaled x255 (an "LSB" here is
/// 1/255 of linear, which under-weights shadows next to an sRGB-encoded 8-bit image): `(mean, max)` over every channel.
fn lsb_error(gpu: &[[f32; 4]], cpu: &[[f32; 3]]) -> (f32, f32) {
    let (mut sum, mut max, mut n) = (0.0f32, 0.0f32, 0usize);
    for (g, c) in gpu.iter().zip(cpu) {
        for ch in 0..3 {
            let d = ((g[ch].clamp(0.0, 1.0) - c[ch].clamp(0.0, 1.0)) * 255.0).abs();
            sum += d;
            max = max.max(d);
            n += 1;
        }
    }
    (sum / n as f32, max)
}

#[test]
fn the_live_suffix_matches_its_cpu_twin_for_every_edit() {
    let Some(gpu) = required_test_gpu() else {
        return;
    };
    let extent = Extent {
        width: W,
        height: H,
    };
    let data = test_image();
    let input = upload_frame(&gpu, extent, &data);
    let output = FrameTexture::new(&gpu, extent);
    let kernel = LiveSuffixKernel::new(&gpu);

    let table = cases();
    assert!(
        table.len() >= 100,
        "the table shrank to {} cases",
        table.len()
    );
    let mut failures = Vec::new();
    let mut worst = (0.0f32, String::new());
    let mut ran = 0usize;
    // Two bases: the camera matrix (negative channels, nothing neutral after it) and the identity
    // matrix (the grey ramp stays achromatic, which reaches the near-grey branches of HSL and
    // vibrance). White-balance cases replace the matrix, so they only make sense on the first.
    for (base_name, base) in [
        ("camera", base_matrix(&WbParams::default())),
        ("identity", color::mat3_identity()),
    ] {
        let render = |params: &LiveParams| {
            kernel.set_params(&gpu, params);
            let mut enc = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            kernel.encode(&gpu, &mut enc, &input, &output);
            gpu.queue.submit(Some(enc.finish()));
            read_frame(&gpu, &output)
        };
        let identity_params = LiveParams {
            working_space_matrix: base,
            ..Default::default()
        };
        let identity_out = render(&identity_params);
        for (name, edit) in &table {
            if base_name == "identity" && name.starts_with("wb ") {
                continue;
            }
            let label = format!("{name} [{base_name}]");
            let mut params = LiveParams {
                working_space_matrix: base,
                ..Default::default()
            };
            edit(&mut params);
            let actual = render(&params);
            ran += 1;
            // A case that changes nothing proves nothing: it must move the GPU image off the
            // identity render (the one deliberate identity case aside).
            if name != "identity" {
                let moved = actual
                    .iter()
                    .zip(&identity_out)
                    .flat_map(|(a, b)| (0..3).map(move |c| (a[c] - b[c]).abs() * 255.0))
                    .fold(0.0f32, f32::max);
                if moved < 0.25 {
                    failures.push(format!("{label}: a silent no-op (moved {moved:.3} LSB)"));
                }
            }
            let expected: Vec<[f32; 3]> = data
                .iter()
                .map(|px| cpu_chain([px[0], px[1], px[2]], &params))
                .collect();
            let (mean, max) = lsb_error(&actual, &expected);
            if max > worst.0 {
                worst = (max, label.clone());
            }
            let allowance = KNOWN_DIVERGENCES.iter().find(|(n, _)| *n == label);
            let max_bound = allowance.map_or(MAX_LSB, |(_, bound)| *bound);
            if mean >= MEAN_LSB || max > max_bound {
                failures.push(format!("{label}: mean {mean:.3} LSB, max {max:.3} LSB"));
            }
            if allowance.is_some() && max <= MAX_LSB {
                failures.push(format!(
                    "{label}: now within {MAX_LSB} LSB (max {max:.3}); delete its KNOWN_DIVERGENCES entry"
                ));
            }
        }
    }
    eprintln!(
        "{ran} cases, worst max error {:.3} LSB ({})",
        worst.0, worst.1
    );
    assert!(
        failures.is_empty(),
        "GPU diverges from the CPU twin (mean < {MEAN_LSB}, max <= {MAX_LSB} LSB):\n{}",
        failures.join("\n")
    );
}

/// How a stage is held against its CPU twin.
enum Coverage {
    /// Checked by the table in this file. For `WB`/`WORKING_SPACE` that means the shader applies
    /// the 3x3 the way `color::mat3_apply` does; the matrix itself is built by the CPU for both
    /// sides, so the gain derivation is covered by `color::`'s own tests, not here. The same
    /// goes for the tone-curve LUT and the exposure multiplier, which both sides share.
    Table,
    /// Checked by a named test in a named source file (the test must exist there).
    Parity {
        source: &'static str,
        test: &'static str,
    },
    /// Nothing to compare, with the reason.
    Exempt(&'static str),
    /// A real stage with no parity test yet; tracked, and the count may only go down.
    Gap(&'static str),
}

const STAGES_SRC: &str = include_str!("stages.rs");
const SLIT_SRC: &str = include_str!("slit.rs");
const HEAL_SRC: &str = include_str!("heal.rs");
const LOCAL_SRC: &str = include_str!("mask/local.rs");
const KERNELS_SRC: &str = include_str!("mask/kernels.rs");

fn source_of(name: &str) -> &'static str {
    match name {
        "stages.rs" => STAGES_SRC,
        "slit.rs" => SLIT_SRC,
        "heal.rs" => HEAL_SRC,
        "mask/local.rs" => LOCAL_SRC,
        "mask/kernels.rs" => KERNELS_SRC,
        other => panic!("unknown source {other}"),
    }
}

/// Gaps that exist today. Lower this when one is closed; a new stage can't add to it silently.
const MAX_GAPS: usize = 3;

const COVERAGE: &[(&str, Coverage)] = &[
    (
        DECODE,
        Coverage::Parity {
            source: "stages.rs",
            test: "decode_gpu_matches_cpu_reference",
        },
    ),
    (
        DEMOSAIC,
        Coverage::Exempt("registry placeholder (BasicStage): no kernel to compare"),
    ),
    (
        DENOISE,
        Coverage::Exempt("registry placeholder (BasicStage): no kernel to compare"),
    ),
    (
        LENS,
        Coverage::Parity {
            source: "slit.rs",
            test: "gpu_matches_the_cpu_reference",
        },
    ),
    (
        HEAL,
        Coverage::Parity {
            source: "heal.rs",
            test: "heal_spot_matches_the_cpu_reference",
        },
    ),
    (WB, Coverage::Table),
    (WORKING_SPACE, Coverage::Table),
    (
        CALIBRATION,
        Coverage::Gap("shadows tint has no CPU-twin parity test (primaries fold into the matrix)"),
    ),
    (
        DEFRINGE,
        Coverage::Parity {
            source: "stages.rs",
            test: "defringe_in_the_live_pass_matches_the_cpu_twin_and_removes_the_fringe",
        },
    ),
    (EXPOSURE, Coverage::Table),
    (TONE, Coverage::Table),
    (TONE_CURVE, Coverage::Table),
    (
        POINT_CURVE,
        Coverage::Parity {
            source: "stages.rs",
            test: "point_curves_in_the_live_pass_match_the_cpu_twin_and_identity_is_a_noop",
        },
    ),
    (VIBRANCE, Coverage::Table),
    (
        PRESENCE,
        Coverage::Parity {
            source: "mask/local.rs",
            test: "global_presence_without_any_mask_matches_the_cpu_twin",
        },
    ),
    (HSL, Coverage::Table),
    (
        COLOR_GRADE,
        Coverage::Parity {
            source: "stages.rs",
            test: "oklab_ops_in_the_live_pass_match_the_cpu_twin_and_neutral_is_a_noop",
        },
    ),
    (
        POINT_COLOR,
        Coverage::Parity {
            source: "stages.rs",
            test: "oklab_ops_in_the_live_pass_match_the_cpu_twin_and_neutral_is_a_noop",
        },
    ),
    (
        SHARPEN,
        Coverage::Gap("no CPU-twin parity test for the global sharpen pass"),
    ),
    (
        NOISE_REDUCTION,
        Coverage::Gap("no CPU-twin parity test for the global noise-reduction pass"),
    ),
    (
        MASKS,
        Coverage::Parity {
            source: "mask/kernels.rs",
            test: "a_whole_group_composed_on_the_gpu_matches_compose_on_the_cpu",
        },
    ),
    (
        CROP,
        Coverage::Parity {
            source: "stages.rs",
            test: "present_sample_gpu_matches_cpu_reference",
        },
    ),
    (
        EFFECTS,
        Coverage::Parity {
            source: "stages.rs",
            test: "present_sample_effects_match_the_cpu_reference",
        },
    ),
];

#[test]
fn every_render_stage_is_classified() {
    // Every production id has an entry, and every entry is a production id.
    let spine: Vec<&str> = BAKED_IDS
        .iter()
        .chain(LIVE_IDS.iter())
        .chain(GEOMETRY_IDS.iter())
        .copied()
        .collect();
    for id in &spine {
        assert!(
            COVERAGE.iter().any(|(c, _)| c == id),
            "stage {id} is in the spine but not classified in equivalence::COVERAGE: add a parity \
             test, a Table case, an Exempt reason or a Gap"
        );
    }
    for (id, _) in COVERAGE {
        assert!(
            spine.contains(id),
            "{id} is classified but not in the spine"
        );
    }
    let mut gaps = 0;
    for (id, cov) in COVERAGE {
        match cov {
            Coverage::Table => {}
            Coverage::Parity { source, test } => {
                // `#[test]` directly above the exact name: not a longer name sharing the prefix, not
                // a comment or string mentioning it, and (with nothing between the two lines) not
                // an `#[ignore]`d one.
                let needle = format!("#[test]\n    fn {test}(");
                assert!(
                    source_of(source).contains(&needle),
                    "{id}: parity test `{test}` is gone from {source}, renamed, or no longer a \
                     plain #[test]"
                );
            }
            Coverage::Exempt(reason) => assert!(!reason.is_empty(), "{id}: empty reason"),
            Coverage::Gap(reason) => {
                assert!(!reason.is_empty(), "{id}: empty reason");
                gaps += 1;
            }
        }
    }
    assert!(
        gaps <= MAX_GAPS,
        "{gaps} parity gaps recorded, more than the {MAX_GAPS} known ones"
    );
    // The Table entries must really be exercised by the table.
    let names: Vec<String> = cases().into_iter().map(|(n, _)| n).collect();
    for needle in [
        "exposure",
        "vibrance",
        "tone ",
        "tone curve",
        "hsl band",
        "wb ",
    ] {
        assert!(
            names.iter().any(|n| n.starts_with(needle)),
            "no table case for `{needle}`"
        );
    }
}

#[test]
fn a_missing_adapter_is_fatal_exactly_when_one_is_required() {
    use crate::test_util::missing_adapter_is_fatal as fatal;
    assert!(fatal(Some("1"), false), "required and absent must fail");
    assert!(fatal(Some("true"), false));
    assert!(!fatal(Some("1"), true), "an adapter is fine");
    assert!(!fatal(Some("0"), false), "0 means not required");
    assert!(!fatal(Some(""), false));
    assert!(!fatal(None, false), "unset keeps the local skip");
}
