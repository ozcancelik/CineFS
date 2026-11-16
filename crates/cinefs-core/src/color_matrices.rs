//! Fallback DNG color calibration matrices (`ColorMatrix1` / `ColorMatrix2`) for known cameras.
//!
//! CineFS always prefers the `color_matrix1` embedded in the MLV `RAWI` block (and
//! `color_matrix2` in the optional `CKCM` block). Only if a clip has an empty, zero,
//! or identity matrix do we consult this table by camera model name or USB ID.

/// EXIF / DNG `CalibrationIlluminant` constants.
pub const ILLUMINANT_STD_A: u16 = 17;
pub const ILLUMINANT_D65: u16 = 21;

/// Dual-illuminant calibration entry (values scaled by 1/10000, matching dcraw / Adobe DNG matrices).
#[derive(Debug, Clone, Copy)]
pub struct CameraColorProfile {
    pub name_pattern: &'static str,
    pub illuminant1: u16,
    pub color_matrix1: [f64; 9],
    pub illuminant2: Option<u16>,
    pub color_matrix2: Option<[f64; 9]>,
}

const PROFILES: &[CameraColorProfile] = &[
    CameraColorProfile {
        name_pattern: "5D Mark III",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.6722, -0.0635, -0.0963, -0.4287, 1.2460, 0.2028, -0.0908, 0.2162, 0.5668],
        illuminant2: Some(ILLUMINANT_STD_A),
        color_matrix2: Some([0.7457, -0.1802, -0.0881, -0.4853, 1.3210, 0.1743, -0.1593, 0.2951, 0.5432]),
    },
    CameraColorProfile {
        name_pattern: "5D Mark II",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.4716, 0.0603, -0.0830, -0.7798, 1.5474, 0.2480, -0.1496, 0.1937, 0.6651],
        illuminant2: None,
        color_matrix2: None,
    },
    CameraColorProfile {
        name_pattern: "EOS M",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.6602, -0.0841, -0.0939, -0.4472, 1.2458, 0.2247, -0.0975, 0.2039, 0.6148],
        illuminant2: None,
        color_matrix2: None,
    },
    CameraColorProfile {
        name_pattern: "70D",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.7034, -0.0804, -0.1014, -0.4420, 1.2564, 0.2058, -0.0851, 0.1994, 0.5758],
        illuminant2: None,
        color_matrix2: None,
    },
    CameraColorProfile {
        name_pattern: "6D",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.7034, -0.0804, -0.1014, -0.4420, 1.2564, 0.2058, -0.0851, 0.1994, 0.5758],
        illuminant2: None,
        color_matrix2: None,
    },
    CameraColorProfile {
        name_pattern: "7D",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.6844, -0.0996, -0.0856, -0.3876, 1.1761, 0.2396, -0.0593, 0.1772, 0.6198],
        illuminant2: None,
        color_matrix2: None,
    },
    CameraColorProfile {
        name_pattern: "60D",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.6719, -0.0994, -0.0925, -0.4408, 1.2426, 0.2211, -0.0887, 0.2129, 0.6051],
        illuminant2: None,
        color_matrix2: None,
    },
    CameraColorProfile {
        name_pattern: "50D",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.4920, 0.0616, -0.0593, -0.6493, 1.3964, 0.2784, -0.1774, 0.3178, 0.7005],
        illuminant2: None,
        color_matrix2: None,
    },
    CameraColorProfile {
        name_pattern: "100D",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.6602, -0.0841, -0.0939, -0.4472, 1.2458, 0.2247, -0.0975, 0.2039, 0.6148],
        illuminant2: None,
        color_matrix2: None,
    },
    CameraColorProfile {
        name_pattern: "700D",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.6602, -0.0841, -0.0939, -0.4472, 1.2458, 0.2247, -0.0975, 0.2039, 0.6148],
        illuminant2: None,
        color_matrix2: None,
    },
    CameraColorProfile {
        name_pattern: "650D",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.6602, -0.0841, -0.0939, -0.4472, 1.2458, 0.2247, -0.0975, 0.2039, 0.6148],
        illuminant2: None,
        color_matrix2: None,
    },
    CameraColorProfile {
        name_pattern: "600D",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.6461, -0.0907, -0.0882, -0.4300, 1.2184, 0.2378, -0.0819, 0.1944, 0.5931],
        illuminant2: None,
        color_matrix2: None,
    },
    CameraColorProfile {
        name_pattern: "550D",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.6941, -0.1164, -0.0857, -0.3825, 1.1597, 0.2534, -0.0416, 0.1540, 0.6039],
        illuminant2: None,
        color_matrix2: None,
    },
    CameraColorProfile {
        name_pattern: "500D",
        illuminant1: ILLUMINANT_D65,
        color_matrix1: [0.4763, 0.0712, -0.0646, -0.6821, 1.4399, 0.2640, -0.1921, 0.3276, 0.6561],
        illuminant2: None,
        color_matrix2: None,
    },
];

/// Looks up a fallback color profile by camera name (case-insensitive substring match).
pub fn lookup_camera_profile(camera_name: &str) -> Option<&'static CameraColorProfile> {
    let lower = camera_name.to_ascii_lowercase();
    PROFILES
        .iter()
        .find(|p| lower.contains(&p.name_pattern.to_ascii_lowercase()))
}

/// Checks whether a 3x3 matrix is usable (non-zero and not an exact identity placeholder).
pub fn is_valid_color_matrix(m: &[f64; 9]) -> bool {
    let any_nonzero = m.iter().any(|&v| v.abs() > 1e-6);
    if !any_nonzero {
        return false;
    }
    let identity = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    let is_identity = m
        .iter()
        .zip(identity.iter())
        .all(|(a, b)| (a - b).abs() < 1e-4);
    !is_identity
}
