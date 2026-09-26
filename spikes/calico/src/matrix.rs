//! Minimal 3x3/3-vector math -- calico only ever needs camera<->XYZ<->working-space matrices and
//! matrix-vector products, so a dedicated linear-algebra dependency isn't warranted.
//!
//! Explicit `[i][j]` indexing (rather than iterator-based rewrites clippy would otherwise
//! suggest) is deliberately kept throughout: it reads closest to the matrix-algebra formulas
//! these functions implement.
#![allow(clippy::needless_range_loop)]

pub type Vec3 = [f64; 3];
/// Row-major 3x3.
pub type Mat3 = [[f64; 3]; 3];

pub const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

pub fn mat_vec_mul(m: &Mat3, v: Vec3) -> Vec3 {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

pub fn mat_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        }
    }
    out
}

pub fn mat_add_scaled(a: &Mat3, b: &Mat3, weight_b: f64) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = a[i][j] * (1.0 - weight_b) + b[i][j] * weight_b;
        }
    }
    out
}

pub fn mat_scale(a: &Mat3, s: f64) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = a[i][j] * s;
        }
    }
    out
}

/// Cramer's-rule 3x3 inverse. Panics on a singular matrix -- every matrix calico inverts (a
/// camera ColorMatrix, or a working-space RGB<->XYZ matrix) is expected non-singular by
/// construction; a singular one indicates a parsing bug worth surfacing loudly rather than
/// silently propagating NaNs.
pub fn mat_invert(m: &Mat3) -> Mat3 {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    assert!(det.abs() > 1e-12, "matrix is singular (det={det})");
    let inv_det = 1.0 / det;
    [
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv_det,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv_det,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv_det,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv_det,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv_det,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv_det,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv_det,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv_det,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv_det,
        ],
    ]
}

pub fn diag(v: Vec3) -> Mat3 {
    [[v[0], 0.0, 0.0], [0.0, v[1], 0.0], [0.0, 0.0, v[2]]]
}

/// The Bradford cone-response matrix and its inverse, used by chromatic adaptation.
const BRADFORD: Mat3 = [
    [0.8951, 0.2664, -0.1614],
    [-0.7502, 1.7135, 0.0367],
    [0.0389, -0.0685, 1.0296],
];

/// Bradford chromatic adaptation from XYZ under `src_white` to XYZ under `dst_white` (both as
/// XYZ tristimulus values normalized to Y=1, i.e. `xyz_from_xy`'s output). DNG's ColorMatrix
/// tags are defined relative to XYZ D50 (the profile connection space); this is what lets calico
/// re-reference that to a working space with a different native white (e.g. ACEScg's D60,
/// Rec.2020/ProPhoto's usual D65 rendition).
pub fn bradford_adapt(src_white: Vec3, dst_white: Vec3) -> Mat3 {
    let inv_bradford = mat_invert(&BRADFORD);
    let src_cone = mat_vec_mul(&BRADFORD, src_white);
    let dst_cone = mat_vec_mul(&BRADFORD, dst_white);
    let scale = diag([
        dst_cone[0] / src_cone[0],
        dst_cone[1] / src_cone[1],
        dst_cone[2] / src_cone[2],
    ]);
    mat_mul(&inv_bradford, &mat_mul(&scale, &BRADFORD))
}

/// CIE xy chromaticity -> XYZ tristimulus, normalized so Y=1.
pub fn xyz_from_xy(x: f64, y: f64) -> Vec3 {
    [x / y, 1.0, (1.0 - x - y) / y]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invert_identity_is_identity() {
        let inv = mat_invert(&IDENTITY);
        for i in 0..3 {
            for j in 0..3 {
                assert!((inv[i][j] - IDENTITY[i][j]).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn invert_round_trips() {
        let m: Mat3 = [[2.0, 0.0, 1.0], [0.0, 3.0, 0.0], [1.0, 0.0, 1.0]];
        let inv = mat_invert(&m);
        let round_trip = mat_mul(&m, &inv);
        for i in 0..3 {
            for j in 0..3 {
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!((round_trip[i][j] - expected).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn bradford_same_white_is_identity() {
        let d50 = xyz_from_xy(0.3457, 0.3585);
        let m = bradford_adapt(d50, d50);
        for i in 0..3 {
            for j in 0..3 {
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!((m[i][j] - expected).abs() < 1e-9);
            }
        }
    }
}
