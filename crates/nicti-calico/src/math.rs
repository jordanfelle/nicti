//! Minimal 3x3 matrix/vector math for the space and adaptation tables. Promoted from
//! `spikes/calico/src/matrix.rs`; only the pieces color management needs.
//!
//! Explicit `[i][j]` indexing is kept on purpose: it reads closest to the matrix formulas.
#![allow(clippy::needless_range_loop)]

pub(crate) type Vec3 = [f64; 3];
/// Row-major 3x3.
pub(crate) type Mat3 = [[f64; 3]; 3];

pub(crate) fn mat_vec_mul(m: &Mat3, v: Vec3) -> Vec3 {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

pub(crate) fn mat_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        }
    }
    out
}

/// Cramer's-rule inverse. Panics on a singular matrix: every matrix inverted here is built from
/// fixed primaries, so a singular one is a programming error worth surfacing loudly.
pub(crate) fn mat_invert(m: &Mat3) -> Mat3 {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    assert!(det.abs() > 1e-12, "matrix is singular (det={det})");
    let d = 1.0 / det;
    [
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * d,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * d,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * d,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * d,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * d,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * d,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * d,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * d,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * d,
        ],
    ]
}

const BRADFORD: Mat3 = [
    [0.8951, 0.2664, -0.1614],
    [-0.7502, 1.7135, 0.0367],
    [0.0389, -0.0685, 1.0296],
];

/// Bradford chromatic adaptation from XYZ under `src_white` to XYZ under `dst_white` (both
/// normalized to Y=1, i.e. [`xyz_from_xy`]'s output).
pub(crate) fn bradford_adapt(src_white: Vec3, dst_white: Vec3) -> Mat3 {
    let src = mat_vec_mul(&BRADFORD, src_white);
    let dst = mat_vec_mul(&BRADFORD, dst_white);
    let scale: Mat3 = [
        [dst[0] / src[0], 0.0, 0.0],
        [0.0, dst[1] / src[1], 0.0],
        [0.0, 0.0, dst[2] / src[2]],
    ];
    mat_mul(&mat_invert(&BRADFORD), &mat_mul(&scale, &BRADFORD))
}

/// CIE xy chromaticity -> XYZ tristimulus, normalized so Y=1.
pub(crate) fn xyz_from_xy(x: f64, y: f64) -> Vec3 {
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// RGB->XYZ matrix (relative to the space's own white) from xy primaries and white point.
pub(crate) fn primaries_to_xyz(primaries: [(f64, f64); 3], white_xy: (f64, f64)) -> Mat3 {
    let c: [Vec3; 3] = primaries.map(|(x, y)| xyz_from_xy(x, y));
    let m: Mat3 = [
        [c[0][0], c[1][0], c[2][0]],
        [c[0][1], c[1][1], c[2][1]],
        [c[0][2], c[1][2], c[2][2]],
    ];
    let s = mat_vec_mul(&mat_invert(&m), xyz_from_xy(white_xy.0, white_xy.1));
    [
        [c[0][0] * s[0], c[1][0] * s[1], c[2][0] * s[2]],
        [c[0][1] * s[0], c[1][1] * s[1], c[2][1] * s[2]],
        [c[0][2] * s[0], c[1][2] * s[1], c[2][2] * s[2]],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invert_round_trips() {
        let m: Mat3 = [[2.0, 0.0, 1.0], [0.0, 3.0, 0.0], [1.0, 0.0, 1.0]];
        let p = mat_mul(&m, &mat_invert(&m));
        for i in 0..3 {
            for j in 0..3 {
                let e = if i == j { 1.0 } else { 0.0 };
                assert!((p[i][j] - e).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn bradford_same_white_is_identity() {
        let d50 = xyz_from_xy(0.3457, 0.3585);
        let m = bradford_adapt(d50, d50);
        for i in 0..3 {
            for j in 0..3 {
                let e = if i == j { 1.0 } else { 0.0 };
                assert!((m[i][j] - e).abs() < 1e-9);
            }
        }
    }
}
