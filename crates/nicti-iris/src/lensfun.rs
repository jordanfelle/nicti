//! lensfun-backed lens profiles (#462): the fallback for lenses Nikon's own 0xC7D5 blob does not
//! cover (adapted F-mount glass on a Z body, third-party lenses, other bodies, files without the
//! tag).
//!
//! The pure-Rust `lensfun` crate (LGPL-3.0-or-later, a port of the C++ library, with the upstream
//! calibration database bundled unmodified under CC BY-SA 3.0, see `docs/licensing.md`) models a
//! lens in its own units and with its own functions: `ptlens`/`poly3`/`poly5` distortion, `linear`/
//! `poly3` TCA and the `pa` vignetting polynomial, normalised by focal length and crop factor.
//! [`LensModel`] is the DNG shape instead (a polynomial in `r²` over the radius normalised to the
//! farthest corner), so this provider does not convert coefficients: it **samples** the lensfun
//! `Modifier` along a ray from the optical centre to the farthest corner (the same
//! point-to-point mapping the renderer needs) and least-squares fits the DNG polynomial to those
//! samples. The fit's worst-case error is checked against [`WARP_TOLERANCE`] /
//! [`VIGNETTE_TOLERANCE`] and a part that does not fit is dropped rather than applied wrongly.
//! `ptlens` has odd powers of `r`, which an `r²` polynomial only approximates; the measured
//! residuals across the bundled database are in `docs/decisions/render-graph.md` (#462).

use std::sync::OnceLock;

use ::lensfun::{Camera, Database, Lens, Modifier};

use crate::{farthest_corner, LensCorrection, LensModel, LensSource, Vignette, Warp};

/// Worst accepted warp fit error, as a fraction of the centre-to-farthest-corner distance (0.7 px
/// at 6000x4000, 1 px at 8256x5504). A distortion fit worse than this is not applied. Real fits are
/// far tighter: the worst off-ray error over the whole bundled database is 1.5e-6.
pub const WARP_TOLERANCE: f64 = 2.0e-4;

/// Worst accepted vignette gain error, relative to the gain itself (1%). Wide-open wide-angle
/// profiles brighten the corners several times over, so an absolute bound would be meaningless.
pub const VIGNETTE_TOLERANCE: f64 = 0.01;

/// Longest EXIF lens name looked up (bytes); see `find`.
pub const MAX_LENS_NAME_BYTES: usize = 160;

/// Sample count along the centre-to-corner ray.
const SAMPLES: usize = 96;

/// Focus distance assumed when the file records none, metres (lensfun treats large values as
/// infinity).
const DEFAULT_DISTANCE_M: f32 = 1000.0;

/// lensfun `Modifier` direction for *correcting* an image (destination point to source point).
const REVERSE: bool = false;

/// The bundled database, parsed once (about 5 MB of XML after decompression). `None` if it fails
/// to parse, which then simply means no lensfun profiles.
fn database() -> Option<&'static Database> {
    static DB: OnceLock<Option<Database>> = OnceLock::new();
    DB.get_or_init(|| Database::load_bundled().ok()).as_ref()
}

/// lensfun provider: matches the EXIF lens model against the bundled database.
pub struct LensfunProvider;

impl LensfunProvider {
    pub const ID: &'static str = "nicti.lens.lensfun";
}

impl nicti_claw::Module for LensfunProvider {
    fn id(&self) -> &str {
        Self::ID
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn migrate_params(
        &self,
        _from_version: u32,
        params: serde_json::Value,
    ) -> Option<serde_json::Value> {
        Some(params)
    }
}

impl LensCorrection for LensfunProvider {
    fn model(&self, source: &LensSource<'_>) -> Option<LensModel> {
        model_from(database()?, source)
    }
}

/// Look up the body and lens named by `source` in `db`.
fn find<'d>(db: &'d Database, source: &LensSource<'_>) -> Option<(&'d Camera, &'d Lens)> {
    // A real lens name is a few dozen bytes. The fuzzy matcher is superlinear in the query length
    // (1 MB takes ~7 s) and the string comes from an untrusted file's EXIF, so refuse long ones.
    if source.lens_model.trim().is_empty() || source.lens_model.len() > MAX_LENS_NAME_BYTES {
        return None;
    }
    let camera = pick_camera(db.find_cameras(Some(source.make), source.model), source)?;
    let lens = db
        .find_lenses(Some(camera), source.lens_model)
        .into_iter()
        .next()?;
    Some((camera, lens))
}

/// The body among fuzzy matches: an exact `"{make} {model}"` or bare `"{model}"` name wins; otherwise the first match
/// only if every match has the same crop factor (a "Z f" query also returns DX bodies, and a wrong
/// crop factor would mis-scale the whole correction).
fn pick_camera<'d>(found: Vec<&'d Camera>, source: &LensSource<'_>) -> Option<&'d Camera> {
    // The database names bodies both ways: "Nikon Z 8" (maker prefixed) and bare "DC-S5".
    let prefixed = format!("{} {}", source.make, source.model);
    if let Some(c) = found.iter().find(|c| {
        c.model.eq_ignore_ascii_case(&prefixed) || c.model.eq_ignore_ascii_case(source.model)
    }) {
        return Some(c);
    }
    let first = *found.first()?;
    found
        .iter()
        .all(|c| (c.crop_factor - first.crop_factor).abs() < 1.0e-3)
        .then_some(first)
}

/// Whether [`LensfunProvider`] can produce a correction for this body, lens, focal length and frame
/// size (what the UI switch is offered on, like `has_nikon_profile`). The UI asks every frame the
/// Lens Corrections section is open, so the last answer is remembered by its inputs; the database
/// is parsed on the first call (tens of ms in a release build).
pub fn has_profile(source: &LensSource<'_>) -> bool {
    use std::sync::Mutex;
    type Key = (
        String,
        String,
        String,
        Option<u32>,
        u32,
        u32,
        Option<u32>,
        Option<u32>,
    );
    static LAST: Mutex<Option<(Key, bool)>> = Mutex::new(None);
    let key: Key = (
        source.make.to_string(),
        source.model.to_string(),
        source.lens_model.to_string(),
        source.focal_length_mm.map(f32::to_bits),
        source.image_width,
        source.image_height,
        source.aperture.map(f32::to_bits),
        source.focus_distance_m.map(f32::to_bits),
    );
    if let Ok(last) = LAST.lock() {
        if let Some((k, v)) = last.as_ref() {
            if *k == key {
                return *v;
            }
        }
    }
    let has = database().is_some_and(|db| model_from(db, source).is_some());
    if let Ok(mut last) = LAST.lock() {
        *last = Some((key, has));
    }
    has
}

/// The fitted [`LensModel`] for `source` against `db`; `None` when the body, lens or focal length
/// is unknown or nothing fits.
pub fn model_from(db: &Database, source: &LensSource<'_>) -> Option<LensModel> {
    let (camera, lens) = find(db, source)?;
    let focal = source
        .focal_length_mm
        .filter(|f| f.is_finite() && *f > 0.0)?;
    let (w, h) = (source.image_width, source.image_height);
    if w < 2 || h < 2 {
        return None;
    }
    let (wf, hf) = (f64::from(w), f64::from(h));
    // lensfun's optical centre in pixel-index coordinates; `Warp::center` is normalised over the
    // image in the edge convention (pixel `i` centred at `i + 0.5`).
    let size = f64::from(w - 1).min(f64::from(h - 1));
    let cx = f64::from(w - 1) / 2.0 + size / 2.0 * f64::from(lens.center_x);
    let cy = f64::from(h - 1) / 2.0 + size / 2.0 * f64::from(lens.center_y);
    let center = [(cx + 0.5) / wf, (cy + 0.5) / hf];
    let m = farthest_corner(wf, hf, cx + 0.5, cy + 0.5);

    // The ray from the centre toward the farthest corner (r = 1 at its end).
    let (cxe, cye) = (cx + 0.5, cy + 0.5);
    let far_x = if cxe > wf / 2.0 { 0.0 } else { wf };
    let far_y = if cye > hf / 2.0 { 0.0 } else { hf };
    let dir = [(far_x - cxe) / m, (far_y - cye) / m];
    let ray = Ray {
        c_idx: [cx, cy],
        dir,
        m,
    };

    let mut modifier = Modifier::new(lens, focal, camera.crop_factor, w, h, REVERSE);
    let has_dist = modifier.enable_distortion_correction(lens);
    let has_tca = modifier.enable_tca_correction(lens);
    let aperture = source.aperture.filter(|a| a.is_finite() && *a > 0.0);
    let has_vig = aperture.is_some_and(|a| {
        let d = source
            .focus_distance_m
            .filter(|d| d.is_finite() && *d > 0.0)
            .unwrap_or(DEFAULT_DISTANCE_M);
        modifier.enable_vignetting_correction(lens, a, d)
    });

    let warp = (has_dist || has_tca)
        .then(|| fit_warp(&modifier, has_tca, center, &ray))
        .flatten();
    let vignette = has_vig
        .then(|| fit_vignette(&modifier, center, &ray))
        .flatten();
    (warp.is_some() || vignette.is_some()).then_some(LensModel { warp, vignette })
}

/// The sampling ray: optical centre in pixel-index coordinates, unit-ish direction (so that
/// `dir * m` reaches the farthest corner) and the centre-to-corner distance `m`.
struct Ray {
    c_idx: [f64; 2],
    dir: [f64; 2],
    m: f64,
}

impl Ray {
    /// Pixel-index point at normalised radius `r`.
    fn at(&self, r: f64) -> (f32, f32) {
        let t = r * self.m;
        (
            (self.c_idx[0] + self.dir[0] * t) as f32,
            (self.c_idx[1] + self.dir[1] * t) as f32,
        )
    }
}

/// Sampled radii `r` in (0, 1] (the exact centre is skipped: 0/0).
fn radii() -> impl Iterator<Item = f64> {
    (1..=SAMPLES).map(|i| i as f64 / SAMPLES as f64)
}

/// Source position of destination point `(x, y)` per channel R, G, B. Geometry is a plain function
/// of a point, so each channel's TCA-shifted position is fed through it in turn (lensfun's own
/// `ApplySubpixelGeometryDistortion` order).
fn source_points(modifier: &Modifier, has_tca: bool, x: f32, y: f32) -> [[f64; 2]; 3] {
    let mut chan = [x, y, x, y, x, y];
    if has_tca {
        modifier.apply_subpixel_distortion(x, y, 1, 1, &mut chan);
    }
    let mut out = [[0.0; 2]; 3];
    for (c, o) in out.iter_mut().enumerate() {
        let mut p = [chan[2 * c], chan[2 * c + 1]];
        // The call fills `q` from the start point and reports false when distortion is not enabled
        // (then the position is already final).
        let mut q = [0.0_f32; 2];
        if modifier.apply_geometry_distortion(p[0], p[1], 1, 1, &mut q) {
            p = q;
        }
        *o = [f64::from(p[0]), f64::from(p[1])];
    }
    out
}

fn fit_warp(modifier: &Modifier, has_tca: bool, center: [f64; 2], ray: &Ray) -> Option<Warp> {
    let mut rows: [Vec<(f64, f64)>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for r in radii() {
        let (x, y) = ray.at(r);
        let (ox, oy) = (f64::from(x) - ray.c_idx[0], f64::from(y) - ray.c_idx[1]);
        let t = ox.hypot(oy);
        let src = source_points(modifier, has_tca, x, y);
        for (c, s) in src.iter().enumerate() {
            let f = (s[0] - ray.c_idx[0]).hypot(s[1] - ray.c_idx[1]) / t;
            if !f.is_finite() {
                return None;
            }
            rows[c].push((r, f));
        }
    }
    let mut planes = Vec::with_capacity(3);
    let mut odd = Vec::with_capacity(3);
    for samples in &rows {
        let k = fit_powers(samples, &WARP_POWERS)?;
        let worst = samples
            .iter()
            .map(|&(r, f)| (eval_powers(&k, &WARP_POWERS, r) - f).abs() * r)
            .fold(0.0, f64::max);
        if worst > WARP_TOLERANCE {
            return None;
        }
        planes.push([k[0], k[1], k[2], k[3], 0.0, 0.0]);
        odd.push([k[4], k[5]]);
    }
    let warp = Warp {
        planes,
        odd,
        center,
    };
    // No per-channel difference: one plane serves all, so the profile does not claim to correct
    // lateral CA (which would switch the automatic estimator off for nothing).
    let warp = if warp.corrects_lateral_ca() {
        warp
    } else {
        Warp {
            planes: vec![warp.planes[1]],
            odd: vec![warp.odd[1]],
            center,
        }
    };
    let identity = warp.planes.iter().all(is_identity)
        && warp.odd.iter().all(|o| o.iter().all(|v| v.abs() < 1.0e-9));
    (!identity).then_some(warp)
}

/// The warp basis for `f(r)`: the DNG even terms `1, r², r⁴, r⁶`, then the odd `r, r³` that
/// `ptlens` needs (`f = a r³ + b r² + c r + d` is then exact, `poly3`/`poly5` are even).
const WARP_POWERS: [u32; 6] = [0, 2, 4, 6, 1, 3];

/// The vignette basis: `r², r⁴, ... r¹⁰` (gain - 1, so no constant term).
const VIGNETTE_POWERS: [u32; 5] = [2, 4, 6, 8, 10];

fn is_identity(p: &[f64; 6]) -> bool {
    const E: f64 = 1.0e-9;
    (p[0] - 1.0).abs() < E && p[1..].iter().all(|v| v.abs() < E)
}

fn fit_vignette(modifier: &Modifier, center: [f64; 2], ray: &Ray) -> Option<Vignette> {
    let mut samples = Vec::with_capacity(SAMPLES);
    for r in radii() {
        let (x, y) = ray.at(r);
        let mut px = [1.0_f32];
        if !modifier.apply_color_modification_f32(&mut px, x, y, 1, 1, 1) {
            return None;
        }
        let gain = f64::from(px[0]);
        if !gain.is_finite() {
            return None;
        }
        // gain = 1 + k0 r² + k1 r⁴ + ..., so fit `gain - 1` over r², r⁴, ... (no constant term).
        samples.push((r, gain - 1.0));
    }
    // A flat profile (no falloff to correct) is no profile.
    if samples.iter().all(|&(_, g)| g.abs() < 1.0e-4) {
        return None;
    }
    let k = fit_powers(&samples, &VIGNETTE_POWERS)?;
    let worst = samples
        .iter()
        .map(|&(r, g)| (eval_powers(&k, &VIGNETTE_POWERS, r) - g).abs() / (1.0 + g).abs())
        .fold(0.0, f64::max);
    (worst <= VIGNETTE_TOLERANCE).then(|| Vignette {
        k: [k[0], k[1], k[2], k[3], k[4]],
        center,
    })
}

/// `sum k[i] * r^powers[i]`.
fn eval_powers(k: &[f64], powers: &[u32], r: f64) -> f64 {
    k.iter()
        .zip(powers)
        .map(|(c, p)| c * r.powi(*p as i32))
        .sum()
}

/// Least-squares fit of `y ~ sum k[i] r^powers[i]` through `samples = (r, y)` by the normal
/// equations (`r` is in (0, 1], so the basis is bounded; Gaussian elimination with partial
/// pivoting in f64 is accurate enough, and every caller checks the residual anyway).
fn fit_powers(samples: &[(f64, f64)], powers: &[u32]) -> Option<Vec<f64>> {
    let n = powers.len();
    let mut a = vec![vec![0.0_f64; n]; n];
    let mut b = vec![0.0_f64; n];
    for &(r, y) in samples {
        let basis: Vec<f64> = powers.iter().map(|p| r.powi(*p as i32)).collect();
        for i in 0..n {
            b[i] += basis[i] * y;
            for j in 0..n {
                a[i][j] += basis[i] * basis[j];
            }
        }
    }
    solve(a, b)
}

fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for col in 0..n {
        let pivot = (col..n).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[pivot][col].abs() < 1.0e-14 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        let (pivot_row, rest) = a[col..].split_first_mut()?;
        let pivot_b = b[col];
        for (offset, row) in rest.iter_mut().enumerate() {
            let f = row[col] / pivot_row[col];
            for (cell, p) in row[col..].iter_mut().zip(&pivot_row[col..]) {
                *cell -= f * p;
            }
            b[col + 1 + offset] -= f * pivot_b;
        }
    }
    let mut x = vec![0.0; n];
    for row in (0..n).rev() {
        let mut s = b[row];
        for k in row + 1..n {
            s -= a[row][k] * x[k];
        }
        x[row] = s / a[row][row];
    }
    x.iter().all(|v| v.is_finite()).then_some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_recovers_an_exact_polynomial_with_odd_terms() {
        let k = [1.0, -0.05, 0.02, -0.003, 0.01, -0.02];
        let samples: Vec<(f64, f64)> = radii()
            .map(|r| (r, eval_powers(&k, &WARP_POWERS, r)))
            .collect();
        let got = fit_powers(&samples, &WARP_POWERS).unwrap();
        for (a, b) in got.iter().zip(&k) {
            assert!((a - b).abs() < 1.0e-6, "{got:?}");
        }
    }

    const XML: &str = r#"<lensdatabase version="2">
  <camera><maker>Acme</maker><model>Acme Body</model><mount>Acme X</mount><cropfactor>1</cropfactor></camera>
  <camera><maker>Acme</maker><model>Acme Body DX</model><mount>Acme X</mount><cropfactor>1.5</cropfactor></camera>
  <lens>
    <maker>Acme</maker><model>Off-centre 24mm</model><mount>Acme X</mount><cropfactor>1</cropfactor>
    <center x="0.12" y="-0.08"/>
    <calibration>
      <distortion model="ptlens" focal="24" a="0.03963" b="-0.12592" c="0.08209"/>
    </calibration>
  </lens>
  <lens>
    <maker>Acme</maker><model>Test 24mm f/2</model><mount>Acme X</mount><cropfactor>1</cropfactor>
    <calibration>
      <distortion model="ptlens" focal="24" a="0.03963" b="-0.12592" c="0.08209"/>
      <tca model="poly3" focal="24" vr="1.0003" vb="0.9997"/>
      <vignetting model="pa" focal="24" aperture="2" distance="1000" k1="-0.32" k2="-0.5" k3="0.2"/>
    </calibration>
  </lens>
</lensdatabase>"#;

    fn db() -> Database {
        let mut db = Database::new();
        db.load_str(XML).unwrap();
        db
    }

    fn source() -> LensSource<'static> {
        LensSource {
            make: "Acme",
            model: "Body",
            lens_model: "Test 24mm f/2",
            focal_length_mm: Some(24.0),
            aperture: Some(2.0),
            focus_distance_m: None,
            image_width: 6000,
            image_height: 4000,
            ..LensSource::default()
        }
    }

    /// The fitted model must reproduce lensfun's own mapping at points *off* the sampling ray
    /// (the fit only saw the diagonal), per channel.
    #[test]
    fn the_fit_matches_lensfun_off_the_sampling_ray() {
        let db = db();
        let src = source();
        let model = model_from(&db, &src).expect("profile");
        let warp = model.warp.expect("warp");
        let (camera, lens) = find(&db, &src).unwrap();
        let (w, h) = (6000_u32, 4000_u32);
        let mut modifier = Modifier::new(lens, 24.0, camera.crop_factor, w, h, REVERSE);
        assert!(modifier.enable_distortion_correction(lens));
        assert!(modifier.enable_tca_correction(lens));
        let (cx, cy) = (f64::from(w) * warp.center[0], f64::from(h) * warp.center[1]);
        let m = farthest_corner(f64::from(w), f64::from(h), cx, cy);
        let mut worst = 0.0_f64;
        for &(x, y) in &[
            (10.0_f32, 3990.0_f32),
            (5990.0, 20.0),
            (3000.0, 30.0),
            (100.0, 2000.0),
            (4500.0, 3000.0),
            (1500.0, 900.0),
        ] {
            let truth = source_points(&modifier, true, x, y);
            for (ch, t) in truth.iter().enumerate() {
                let (dx, dy) = (f64::from(x) + 0.5 - cx, f64::from(y) + 0.5 - cy);
                let (sx, sy) = warp.source_offset(ch, dx, dy, m);
                // lensfun works in pixel-index coordinates, the model in edge ones (+0.5).
                let got = (cx + sx - 0.5, cy + sy - 0.5);
                worst = worst.max((got.0 - t[0]).hypot(got.1 - t[1]));
            }
        }
        assert!(worst < 0.75, "worst source error {worst} px");
    }

    /// `ptlens` with these coefficients is barrel-shaped at the edge, so correcting it samples
    /// inside the frame (`f < 1` at the corner): the direction convention, pinned.
    #[test]
    fn a_barrel_lens_samples_inside_the_frame_and_a_vignette_brightens_the_corners() {
        let model = model_from(&db(), &source()).expect("profile");
        let warp = model.warp.unwrap();
        let corner = (3000.0_f64, 2000.0_f64);
        let m = corner.0.hypot(corner.1);
        let (sx, sy) = warp.source_offset(1, corner.0, corner.1, m);
        assert!(sx.hypot(sy) < m, "{sx} {sy}");
        let v = model.vignette.expect("vignette");
        assert!(v.gain(1.0) > 1.2, "corner gain {}", v.gain(1.0));
        assert!((v.gain(0.0) - 1.0).abs() < 1.0e-12);
    }

    #[test]
    fn lateral_ca_is_reported_only_when_the_lens_has_tca() {
        let model = model_from(&db(), &source()).unwrap();
        assert!(model.warp.unwrap().corrects_lateral_ca());
    }

    #[test]
    fn unknown_inputs_give_no_profile() {
        let db = db();
        let ok = source();
        assert!(model_from(&db, &ok).is_some());
        for bad in [
            LensSource {
                lens_model: "",
                ..ok
            },
            LensSource {
                lens_model: "No Such Lens 9000mm",
                ..ok
            },
            LensSource {
                model: "Nonexistent",
                ..ok
            },
            LensSource {
                focal_length_mm: None,
                ..ok
            },
            LensSource {
                focal_length_mm: Some(f32::NAN),
                ..ok
            },
            LensSource {
                focal_length_mm: Some(0.0),
                ..ok
            },
            LensSource {
                image_width: 0,
                ..ok
            },
            LensSource {
                image_height: 1,
                ..ok
            },
        ] {
            assert!(model_from(&db, &bad).is_none(), "{bad:?}");
        }
        // No aperture: distortion still applies, only the vignette is skipped.
        let m = model_from(
            &db,
            &LensSource {
                aperture: None,
                ..ok
            },
        )
        .unwrap();
        assert!(m.warp.is_some() && m.vignette.is_none());
    }

    #[test]
    fn an_exact_body_name_wins_over_a_fuzzy_match_with_another_crop_factor() {
        let db = db();
        let src = LensSource {
            model: "Body",
            ..source()
        };
        let found = db.find_cameras(Some("Acme"), "Body");
        assert!(found.len() > 1, "fixture needs an ambiguous query");
        assert_eq!(pick_camera(found, &src).unwrap().model, "Acme Body");
        // Ambiguous with different crop factors and no exact name: refuse rather than guess.
        let src = LensSource {
            model: "Bod",
            ..source()
        };
        assert!(pick_camera(db.find_cameras(Some("Acme"), "Bod"), &src).is_none());
    }

    #[test]
    fn the_bundled_database_serves_a_real_nikon_z_lens() {
        let src = LensSource {
            make: "Nikon",
            model: "Z 8",
            lens_model: "NIKKOR Z 24-70mm f/2.8 S",
            focal_length_mm: Some(24.0),
            aperture: Some(4.0),
            focus_distance_m: Some(5.0),
            image_width: 8256,
            image_height: 5504,
            ..LensSource::default()
        };
        assert!(has_profile(&src));
        let model = LensfunProvider.model(&src).expect("profile");
        let warp = model.warp.expect("warp");
        assert!(
            warp.corrects_lateral_ca(),
            "the database has TCA for this lens"
        );
        let m = farthest_corner(8256.0, 5504.0, 4128.0, 2752.0);
        let (sx, sy) = warp.source_offset(1, 4128.0, 2752.0, m);
        assert!(
            sx.hypot(sy) < m,
            "24 mm is barrel: the corner samples inside"
        );
        assert!(model.vignette.expect("vignette").gain(1.0) > 1.0);
    }

    /// Coverage census over the whole bundled database (run with `--ignored --nocapture`): fit
    /// every calibrated distortion focal length and every calibrated vignetting entry at 6000x4000
    /// and count what the tolerances accept. The numbers are recorded in
    /// `docs/decisions/render-graph.md`.
    #[test]
    #[ignore]
    fn census() {
        let db = Database::load_bundled().unwrap();
        let ray = Ray {
            c_idx: [2999.5, 1999.5],
            dir: [3000.0 / 3605.55, 2000.0 / 3605.55],
            m: 3605.55,
        };
        let (mut dist, mut dist_ok, mut vig, mut vig_ok) = (0, 0, 0, 0);
        for lens in &db.lenses {
            for d in &lens.calib_distortion {
                let mut m = Modifier::new(lens, d.focal, lens.crop_factor, 6000, 4000, REVERSE);
                if !m.enable_distortion_correction(lens) {
                    continue;
                }
                let tca = m.enable_tca_correction(lens);
                dist += 1;
                dist_ok += usize::from(fit_warp(&m, tca, [0.5, 0.5], &ray).is_some());
            }
            for v in &lens.calib_vignetting {
                let mut m = Modifier::new(lens, v.focal, lens.crop_factor, 6000, 4000, REVERSE);
                if !m.enable_vignetting_correction(lens, v.aperture, v.distance) {
                    continue;
                }
                vig += 1;
                vig_ok += usize::from(fit_vignette(&m, [0.5, 0.5], &ray).is_some());
            }
        }
        println!("census: warp {dist_ok}/{dist} fitted, vignette {vig_ok}/{vig} fitted");
    }

    /// The real database names some bodies without the maker ("DC-S5") and a query for one also
    /// returns neighbours with other crop factors (DC-GH5S, crop 2): the exact bare name must win.
    #[test]
    fn a_bare_exact_body_name_beats_its_fuzzy_neighbours() {
        let db = Database::load_bundled().unwrap();
        let src = LensSource {
            make: "Panasonic",
            model: "DC-S5",
            ..LensSource::default()
        };
        let found = db.find_cameras(Some("Panasonic"), "DC-S5");
        assert!(
            found.len() > 1,
            "the query is ambiguous in the real database"
        );
        let picked = pick_camera(found, &src).expect("exact bare name");
        assert_eq!(picked.model, "DC-S5");
    }

    #[test]
    fn an_offcentre_lens_fits_and_matches_lensfun_off_the_ray() {
        let db = db();
        let src = LensSource {
            lens_model: "Off-centre 24mm",
            aperture: None,
            ..source()
        };
        let warp = model_from(&db, &src).expect("profile").warp.expect("warp");
        assert!((warp.center[0] - 0.5).abs() > 0.01, "{:?}", warp.center);
        let (camera, lens) = find(&db, &src).unwrap();
        let (w, h) = (6000_u32, 4000_u32);
        let mut modifier = Modifier::new(lens, 24.0, camera.crop_factor, w, h, REVERSE);
        assert!(modifier.enable_distortion_correction(lens));
        let (cx, cy) = (f64::from(w) * warp.center[0], f64::from(h) * warp.center[1]);
        let m = farthest_corner(f64::from(w), f64::from(h), cx, cy);
        let mut worst = 0.0_f64;
        for &(x, y) in &[
            (10.0_f32, 3990.0_f32),
            (5990.0, 20.0),
            (3000.0, 30.0),
            (4500.0, 3000.0),
        ] {
            let t = source_points(&modifier, false, x, y)[1];
            let (dx, dy) = (f64::from(x) + 0.5 - cx, f64::from(y) + 0.5 - cy);
            let (sx, sy) = warp.source_offset(1, dx, dy, m);
            worst = worst.max((cx + sx - 0.5 - t[0]).hypot(cy + sy - 0.5 - t[1]));
        }
        assert!(worst < 0.75, "worst source error {worst} px");
    }

    #[test]
    fn the_switch_needs_a_usable_profile_and_hostile_names_are_refused() {
        let real = LensSource {
            make: "Nikon",
            model: "Z 8",
            lens_model: "NIKKOR Z 24-70mm f/2.8 S",
            focal_length_mm: Some(24.0),
            aperture: Some(4.0),
            image_width: 8256,
            image_height: 5504,
            ..LensSource::default()
        };
        assert!(has_profile(&real));
        // Known body and lens but no focal length: nothing can be applied, so no switch.
        assert!(!has_profile(&LensSource {
            focal_length_mm: None,
            ..real
        }));
        let long = "NIKKOR Z 24-70mm f/2.8 S".repeat(10_000);
        let t = std::time::Instant::now();
        assert!(!has_profile(&LensSource {
            lens_model: &long,
            ..real
        }));
        assert!(t.elapsed().as_millis() < 500, "{:?}", t.elapsed());
    }

    #[test]
    fn a_singular_system_is_none_not_nan() {
        assert!(fit_powers(&[], &WARP_POWERS).is_none());
    }
}
