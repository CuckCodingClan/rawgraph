//! Colour math: primaries, chromatic adaptation, transfer functions.
//!
//! Working space is ACEScg (AP1 primaries, ACES white ~D60), linear.
//! Matrices are derived in f64 from primaries instead of being pasted in, and
//! the tests pin them against published values.

pub type Mat3 = [[f64; 3]; 3];

/// ACEScg (AP1) RGB -> CIE XYZ, ACES white.
pub const AP1_TO_XYZ: Mat3 = [
    [0.662_454_181_1, 0.134_004_206_5, 0.156_187_687_0],
    [0.272_228_716_8, 0.674_081_765_8, 0.053_689_517_4],
    [-0.005_574_649_5, 0.004_060_733_5, 1.010_339_100_3],
];

/// Linear Rec.709 / sRGB RGB -> CIE XYZ, D65.
pub const REC709_TO_XYZ: Mat3 = [
    [0.412_390_8, 0.357_584_3, 0.180_480_8],
    [0.212_639_0, 0.715_168_7, 0.072_192_3],
    [0.019_330_8, 0.119_194_8, 0.950_532_2],
];

/// Bradford cone response matrix.
pub const BRADFORD: Mat3 = [
    [0.8951, 0.2664, -0.1614],
    [-0.7502, 1.7135, 0.0367],
    [0.0389, -0.0685, 1.0296],
];

pub const D65_XY: (f64, f64) = (0.3127, 0.3290);
pub const ACES_WHITE_XY: (f64, f64) = (0.32168, 0.33767);

/// Luminance weights of ACEScg (the Y row of `AP1_TO_XYZ`).
pub const AP1_LUMA: [f32; 3] = [0.272_228_7, 0.674_081_8, 0.053_689_5];
/// Rec.709 luma weights, used on display-encoded R'G'B' for scopes.
pub const REC709_LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

pub fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut m = [[0.0; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            m[r][c] = (0..3).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    m
}

pub fn mul_vec(m: &Mat3, v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

pub fn invert(m: &Mat3) -> Mat3 {
    let c = |r1: usize, c1: usize, r2: usize, c2: usize| m[r1][c1] * m[r2][c2] - m[r1][c2] * m[r2][c1];
    let det = m[0][0] * c(1, 1, 2, 2) - m[0][1] * c(1, 0, 2, 2) + m[0][2] * c(1, 0, 2, 1);
    let d = 1.0 / det;
    [
        [c(1, 1, 2, 2) * d, -c(0, 1, 2, 2) * d, c(0, 1, 1, 2) * d],
        [-c(1, 0, 2, 2) * d, c(0, 0, 2, 2) * d, -c(0, 0, 1, 2) * d],
        [c(1, 0, 2, 1) * d, -c(0, 0, 2, 1) * d, c(0, 0, 1, 1) * d],
    ]
}

pub fn to_f32(m: &Mat3) -> [[f32; 3]; 3] {
    let mut o = [[0.0f32; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            o[r][c] = m[r][c] as f32;
        }
    }
    o
}

/// Chromaticity -> XYZ with Y = 1.
pub fn xy_to_xyz((x, y): (f64, f64)) -> [f64; 3] {
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// Bradford chromatic adaptation from `src` white to `dst` white (both XYZ).
pub fn adaptation(src_white: [f64; 3], dst_white: [f64; 3]) -> Mat3 {
    let s = mul_vec(&BRADFORD, src_white);
    let d = mul_vec(&BRADFORD, dst_white);
    let scale: Mat3 = [[d[0] / s[0], 0.0, 0.0], [0.0, d[1] / s[1], 0.0], [0.0, 0.0, d[2] / s[2]]];
    mul(&invert(&BRADFORD), &mul(&scale, &BRADFORD))
}

/// Linear Rec.709 (D65) -> ACEScg.
pub fn rec709_to_acescg() -> Mat3 {
    let cat = adaptation(xy_to_xyz(D65_XY), xy_to_xyz(ACES_WHITE_XY));
    mul(&invert(&AP1_TO_XYZ), &mul(&cat, &REC709_TO_XYZ))
}

/// ACEScg -> linear Rec.709 (D65).
pub fn acescg_to_rec709() -> Mat3 {
    invert(&rec709_to_acescg())
}

/// sRGB opto-electronic transfer function (linear -> encoded). Passes negatives through mirrored.
pub fn srgb_oetf(x: f32) -> f32 {
    if x < 0.0 {
        -srgb_oetf(-x)
    } else if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB electro-optical transfer function (encoded -> linear).
pub fn srgb_eotf(x: f32) -> f32 {
    if x < 0.0 {
        -srgb_eotf(-x)
    } else if x <= 0.040_45 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

/// Chromaticity of a Planckian radiator (Kang et al. 2002), valid 1667 K..25000 K.
pub fn cct_to_xy(kelvin: f64) -> (f64, f64) {
    let t = kelvin.clamp(1667.0, 25000.0);
    let x = if t <= 4000.0 {
        -0.266_123_9e9 / t.powi(3) - 0.234_358_9e6 / t.powi(2) + 0.877_695_6e3 / t + 0.179_910
    } else {
        -3.025_846_9e9 / t.powi(3) + 2.107_037_9e6 / t.powi(2) + 0.222_634_7e3 / t + 0.240_390
    };
    let y = if t <= 2222.0 {
        -1.106_381_4 * x.powi(3) - 1.348_110_20 * x.powi(2) + 2.185_558_32 * x - 0.202_196_83
    } else if t <= 4000.0 {
        -0.954_947_6 * x.powi(3) - 1.374_185_93 * x.powi(2) + 2.091_370_15 * x - 0.167_488_67
    } else {
        3.081_758_0 * x.powi(3) - 5.873_386_70 * x.powi(2) + 3.751_129_97 * x - 0.370_014_83
    };
    (x, y)
}

/// tint = TINT_SCALE * Duv, with Duv the signed CIE 1960 uv distance from the
/// Planckian locus, positive towards magenta (below the locus). A positive
/// tint therefore names a white point on the green side of the locus; making
/// such a white neutral adds magenta to the image, as a raw converter's tint
/// slider does (DNG SDK convention).
pub const TINT_SCALE: f64 = -3000.0;

const MIRED_MIN: f64 = 1.0e6 / 25000.0;
const MIRED_MAX: f64 = 1.0e6 / 1667.0;

fn xy_to_uv((x, y): (f64, f64)) -> (f64, f64) {
    let d = -2.0 * x + 12.0 * y + 3.0;
    (4.0 * x / d, 6.0 * y / d)
}

fn uv_to_xy((u, v): (f64, f64)) -> (f64, f64) {
    let d = 2.0 * u - 8.0 * v + 4.0;
    (3.0 * u / d, 2.0 * v / d)
}

/// Locus point in CIE 1960 uv at a mired value.
fn locus_uv(mired: f64) -> (f64, f64) {
    xy_to_uv(cct_to_xy(1.0e6 / mired.clamp(MIRED_MIN, MIRED_MAX)))
}

/// Unit normal of the locus pointing to the green side (larger v).
fn locus_normal_up(mired: f64) -> (f64, f64) {
    let h = 0.5;
    let a = locus_uv(mired - h);
    let b = locus_uv(mired + h);
    let (du, dv) = (b.0 - a.0, b.1 - a.1);
    let len = du.hypot(dv).max(1e-300);
    let (nx, ny) = (-dv / len, du / len);
    if ny >= 0.0 {
        (nx, ny)
    } else {
        (-nx, -ny)
    }
}

/// White point (CIE xy) for a correlated colour temperature and tint.
/// Tint 0 lies on the Planckian locus; kelvin is clamped to 1667..25000.
pub fn kelvin_tint_to_xy(kelvin: f64, tint: f64) -> (f64, f64) {
    let mired = 1.0e6 / kelvin.clamp(1667.0, 25000.0);
    let (u, v) = locus_uv(mired);
    let (nx, ny) = locus_normal_up(mired);
    // Duv is positive below the locus; positive tint therefore sits above it.
    let up = -(tint / TINT_SCALE);
    uv_to_xy((u + nx * up, v + ny * up))
}

/// Inverse of [`kelvin_tint_to_xy`]: nearest Planckian locus point in CIE 1960
/// uv gives the temperature, the signed distance to it the tint.
pub fn xy_to_kelvin_tint(xy: (f64, f64)) -> (f64, f64) {
    let (pu, pv) = xy_to_uv(xy);
    let dist2 = |m: f64| {
        let (u, v) = locus_uv(m);
        (pu - u).powi(2) + (pv - v).powi(2)
    };
    const STEPS: usize = 480;
    let step = (MIRED_MAX - MIRED_MIN) / STEPS as f64;
    let mut best = 0;
    let mut best_d = f64::INFINITY;
    for i in 0..=STEPS {
        let d = dist2(MIRED_MIN + step * i as f64);
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    // Golden-section refinement around the best grid point.
    let mut lo = (MIRED_MIN + step * (best as f64 - 1.0)).max(MIRED_MIN);
    let mut hi = (MIRED_MIN + step * (best as f64 + 1.0)).min(MIRED_MAX);
    let g = 0.618_033_988_749_894_9;
    let (mut a, mut b) = (hi - g * (hi - lo), lo + g * (hi - lo));
    let (mut fa, mut fb) = (dist2(a), dist2(b));
    for _ in 0..80 {
        if fa < fb {
            hi = b;
            b = a;
            fb = fa;
            a = hi - g * (hi - lo);
            fa = dist2(a);
        } else {
            lo = a;
            a = b;
            fa = fb;
            b = lo + g * (hi - lo);
            fb = dist2(b);
        }
    }
    let mired = 0.5 * (lo + hi);
    let (u, v) = locus_uv(mired);
    let (nx, ny) = locus_normal_up(mired);
    let up = (pu - u) * nx + (pv - v) * ny;
    (1.0e6 / mired, -up * TINT_SCALE)
}

/// ACEScg matrix that re-balances an image developed for the `as_shot`
/// (kelvin, tint) white so that `target` (kelvin, tint) becomes neutral.
///
/// Like a raw converter's sliders: a higher target temperature warms the image,
/// a higher target tint moves it towards magenta. Identity when both are equal.
pub fn absolute_white_balance_matrix(as_shot: (f64, f64), target: (f64, f64)) -> Mat3 {
    let w_ref = xy_to_xyz(ACES_WHITE_XY);
    let w_as = xy_to_xyz(kelvin_tint_to_xy(as_shot.0, as_shot.1));
    let w_t = xy_to_xyz(kelvin_tint_to_xy(target.0, target.1));
    // How the target white appears in the as-shot-balanced image.
    let c = mul_vec(&adaptation(w_as, w_ref), w_t);
    mul(&invert(&AP1_TO_XYZ), &mul(&adaptation(c, w_ref), &AP1_TO_XYZ))
}

/// Reference temperature for relative white-balance shifts.
pub const WB_REFERENCE_KELVIN: f64 = 6500.0;
/// Stops of green gain per tint unit (provisional scale).
pub const WB_TINT_STOPS_PER_UNIT: f64 = 0.003;

/// Relative white-balance matrix in ACEScg.
///
/// `mired_shift` > 0 warms the image, exactly like raising the temperature
/// slider in a raw converter: the assumed scene illuminant moves to a higher
/// colour temperature and is adapted back to the reference white.
/// `tint` > 0 moves towards magenta. Zero for both is the identity, so
/// relative nodes compose after an absolute white balance.
pub fn white_balance_matrix(mired_shift: f64, tint: f64) -> Mat3 {
    let ref_mired = 1.0e6 / WB_REFERENCE_KELVIN;
    let src_kelvin = 1.0e6 / (ref_mired - mired_shift).max(40.0);
    let src = xy_to_xyz(cct_to_xy(src_kelvin));
    let dst = xy_to_xyz(cct_to_xy(WB_REFERENCE_KELVIN));
    let cat = adaptation(src, dst);
    let mut m = mul(&invert(&AP1_TO_XYZ), &mul(&cat, &AP1_TO_XYZ));
    let g = 2f64.powf(-tint * WB_TINT_STOPS_PER_UNIT);
    for v in &mut m[1] {
        *v *= g;
    }
    m
}

/// Labels of the output colour spaces, indexed like [`OutputSpace::from_index`].
pub const OUTPUT_SPACES: [&str; 4] = ["sRGB", "Display P3", "Adobe RGB (1998)", "Rec. 2020"];

/// An RGB colour space an image can be written or displayed in. All four use
/// the D65 white point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum OutputSpace {
    #[default]
    Srgb,
    DisplayP3,
    AdobeRgb,
    Rec2020,
}

/// Transfer function of an output space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Transfer {
    /// The piecewise sRGB curve.
    Srgb,
    /// A pure power law with this exponent (encoded = linear^(1/gamma)).
    Gamma(f64),
}

impl OutputSpace {
    pub const ALL: [OutputSpace; 4] =
        [OutputSpace::Srgb, OutputSpace::DisplayP3, OutputSpace::AdobeRgb, OutputSpace::Rec2020];

    /// The space with this index in [`OUTPUT_SPACES`]; anything out of range gives sRGB.
    pub fn from_index(i: usize) -> OutputSpace {
        Self::ALL.get(i).copied().unwrap_or_default()
    }

    /// Index in [`OUTPUT_SPACES`].
    pub fn index(self) -> usize {
        match self {
            OutputSpace::Srgb => 0,
            OutputSpace::DisplayP3 => 1,
            OutputSpace::AdobeRgb => 2,
            OutputSpace::Rec2020 => 3,
        }
    }

    pub fn label(self) -> &'static str {
        OUTPUT_SPACES[self.index()]
    }

    /// CIE xy of the red, green and blue primaries. The white point is D65 ([`D65_XY`]).
    pub fn primaries(self) -> [(f64, f64); 3] {
        match self {
            OutputSpace::Srgb => [(0.640, 0.330), (0.300, 0.600), (0.150, 0.060)],
            OutputSpace::DisplayP3 => [(0.680, 0.320), (0.265, 0.690), (0.150, 0.060)],
            OutputSpace::AdobeRgb => [(0.640, 0.330), (0.210, 0.710), (0.150, 0.060)],
            OutputSpace::Rec2020 => [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)],
        }
    }

    pub fn transfer(self) -> Transfer {
        match self {
            OutputSpace::Srgb | OutputSpace::DisplayP3 => Transfer::Srgb,
            OutputSpace::AdobeRgb => Transfer::Gamma(563.0 / 256.0),
            OutputSpace::Rec2020 => Transfer::Gamma(2.4),
        }
    }

    /// Linear -> encoded. Negative values are mirrored (like [`srgb_oetf`]).
    pub fn encode(self, x: f32) -> f32 {
        match self.transfer() {
            Transfer::Srgb => srgb_oetf(x),
            Transfer::Gamma(g) => {
                if x < 0.0 {
                    -self.encode(-x)
                } else {
                    x.powf((1.0 / g) as f32)
                }
            }
        }
    }
}

/// RGB -> XYZ matrix of a space given by its primaries and white point (xy), Y of white = 1.
pub fn rgb_to_xyz(primaries: [(f64, f64); 3], white: (f64, f64)) -> Mat3 {
    let cols = primaries.map(|(x, y)| {
        let y = if y.abs() < 1e-12 { 1e-12 } else { y };
        xy_to_xyz((x, y))
    });
    // Rows of `p` are the primaries as XYZ columns.
    let p: Mat3 = [
        [cols[0][0], cols[1][0], cols[2][0]],
        [cols[0][1], cols[1][1], cols[2][1]],
        [cols[0][2], cols[1][2], cols[2][2]],
    ];
    let s = mul_vec(&invert(&p), xy_to_xyz(white));
    [
        [p[0][0] * s[0], p[0][1] * s[1], p[0][2] * s[2]],
        [p[1][0] * s[0], p[1][1] * s[1], p[1][2] * s[2]],
        [p[2][0] * s[0], p[2][1] * s[1], p[2][2] * s[2]],
    ]
}

const IDENTITY3: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// ACEScg -> linear `space` (Bradford from the ACES white to D65). For sRGB this
/// is exactly [`acescg_to_rec709`].
pub fn acescg_to_output(space: OutputSpace) -> Mat3 {
    if space == OutputSpace::Srgb {
        return acescg_to_rec709();
    }
    let cat = adaptation(xy_to_xyz(ACES_WHITE_XY), xy_to_xyz(D65_XY));
    let xyz_to_rgb = invert(&rgb_to_xyz(space.primaries(), D65_XY));
    mul(&xyz_to_rgb, &mul(&cat, &AP1_TO_XYZ))
}

/// Linear Rec.709 -> linear `space` (identity for sRGB). It is the exact
/// counterpart of [`acescg_to_output`]: `rec709_to_output(s) = acescg_to_output(s) * rec709_to_acescg()`.
pub fn rec709_to_output(space: OutputSpace) -> Mat3 {
    if space == OutputSpace::Srgb {
        return IDENTITY3;
    }
    mul(&acescg_to_output(space), &rec709_to_acescg())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &Mat3, b: &Mat3, eps: f64) -> bool {
        (0..3).all(|r| (0..3).all(|c| (a[r][c] - b[r][c]).abs() < eps))
    }

    const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

    #[test]
    fn rec709_to_acescg_matches_published_matrix() {
        let published: Mat3 = [
            [0.613_097, 0.339_523, 0.047_379],
            [0.070_194, 0.916_354, 0.013_452],
            [0.020_616, 0.109_570, 0.869_815],
        ];
        assert!(close(&rec709_to_acescg(), &published, 2e-3), "{:?}", rec709_to_acescg());
    }

    #[test]
    fn white_stays_white_between_rec709_and_acescg() {
        let w = mul_vec(&rec709_to_acescg(), [1.0, 1.0, 1.0]);
        assert!(w.iter().all(|v| (v - 1.0).abs() < 1e-3), "{w:?}");
    }

    #[test]
    fn inverse_roundtrips() {
        assert!(close(&mul(&rec709_to_acescg(), &acescg_to_rec709()), &IDENTITY, 1e-12));
    }

    #[test]
    fn srgb_transfer_roundtrips_and_hits_known_points() {
        for v in [0.0f32, 0.001, 0.18, 0.5, 1.0, 4.0] {
            assert!((srgb_eotf(srgb_oetf(v)) - v).abs() < 1e-4 * v.max(1.0));
        }
        assert!((srgb_oetf(0.18) - 0.4614).abs() < 1e-3);
        assert!((srgb_oetf(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn planckian_locus_known_points() {
        let (x, y) = cct_to_xy(6500.0);
        assert!((x - 0.3135).abs() < 2e-3 && (y - 0.3237).abs() < 2e-3, "{x} {y}");
        let (x, y) = cct_to_xy(3000.0);
        assert!((x - 0.4369).abs() < 2e-3 && (y - 0.4041).abs() < 2e-3, "{x} {y}");
    }

    #[test]
    fn kelvin_tint_round_trips() {
        let mut k = 2000.0;
        while k <= 20000.0 {
            for tint in [-100.0, -50.0, -10.0, 0.0, 7.0, 40.0, 100.0] {
                let xy = kelvin_tint_to_xy(k, tint);
                let (k2, t2) = xy_to_kelvin_tint(xy);
                assert!((k2 - k).abs() <= 0.002 * k, "{k} K tint {tint}: got {k2} K");
                assert!((t2 - tint).abs() <= 0.5, "{k} K tint {tint}: got tint {t2}");
            }
            k *= 1.07;
        }
        for k in [2000.0, 3200.0, 4000.0, 5000.0, 6500.0, 10000.0, 20000.0] {
            let (k2, t2) = xy_to_kelvin_tint(kelvin_tint_to_xy(k, 0.0));
            assert!((k2 - k).abs() <= 0.002 * k && t2.abs() <= 0.5, "{k}: {k2} {t2}");
        }
    }

    #[test]
    fn tint_zero_is_on_the_locus_and_sign_is_green_for_positive() {
        let on = kelvin_tint_to_xy(5000.0, 0.0);
        let want = cct_to_xy(5000.0);
        assert!((on.0 - want.0).abs() < 1e-9 && (on.1 - want.1).abs() < 1e-9);
        assert!(kelvin_tint_to_xy(5000.0, 50.0).1 > on.1, "positive tint = green-side white point");
        assert!(kelvin_tint_to_xy(5000.0, -50.0).1 < on.1);
        // Magnitude: tint 30 is Duv 0.01 in CIE 1960 uv.
        let (u0, v0) = xy_to_uv(on);
        let (u1, v1) = xy_to_uv(kelvin_tint_to_xy(5000.0, 30.0));
        assert!(((u1 - u0).hypot(v1 - v0) - 0.01).abs() < 5e-4);
    }

    fn neutral_after(m: &Mat3) -> [f64; 3] {
        mul_vec(m, [1.0, 1.0, 1.0])
    }

    #[test]
    fn absolute_wb_is_identity_when_target_equals_as_shot() {
        for wb in [(6500.0, 0.0), (3200.0, 25.0), (12000.0, -40.0)] {
            assert!(close(&absolute_white_balance_matrix(wb, wb), &IDENTITY, 1e-9), "{wb:?}");
            let g = neutral_after(&absolute_white_balance_matrix(wb, wb));
            assert!(g.iter().all(|v| (v - 1.0).abs() < 1e-9), "{g:?}");
        }
    }

    #[test]
    fn absolute_wb_cools_for_low_targets_and_warms_for_high_ones() {
        let blue = neutral_after(&absolute_white_balance_matrix((6500.0, 0.0), (3200.0, 0.0)));
        assert!(blue[2] > 1.3 * blue[0], "{blue:?}");
        let warm = neutral_after(&absolute_white_balance_matrix((3200.0, 0.0), (6500.0, 0.0)));
        assert!(warm[0] > 1.3 * warm[2], "{warm:?}");
        // Monotonic in the target temperature.
        let r = |t: f64| {
            let n = neutral_after(&absolute_white_balance_matrix((5500.0, 0.0), (t, 0.0)));
            n[0] / n[2]
        };
        assert!(r(4000.0) < r(5500.0) && r(5500.0) < r(8000.0));
    }

    #[test]
    fn absolute_wb_tint_moves_towards_magenta_and_back() {
        let m = neutral_after(&absolute_white_balance_matrix((5500.0, 0.0), (5500.0, 40.0)));
        assert!(m[1] < m[0] && m[1] < m[2], "{m:?}");
        let g = neutral_after(&absolute_white_balance_matrix((5500.0, 0.0), (5500.0, -40.0)));
        assert!(g[1] > g[0] && g[1] > g[2], "{g:?}");
    }

    #[test]
    fn absolute_wb_makes_the_target_white_neutral() {
        // An image shot at 6500 K: a patch with the colour of a 3200 K illuminant, as the
        // camera recorded it, becomes neutral when 3200 K is the target.
        let as_shot = (6500.0, 0.0);
        let target = (3200.0, 0.0);
        let w_ref = xy_to_xyz(ACES_WHITE_XY);
        let seen = mul_vec(
            &adaptation(xy_to_xyz(kelvin_tint_to_xy(as_shot.0, as_shot.1)), w_ref),
            xy_to_xyz(kelvin_tint_to_xy(target.0, target.1)),
        );
        let rgb = mul_vec(&invert(&AP1_TO_XYZ), seen);
        let out = mul_vec(&absolute_white_balance_matrix(as_shot, target), rgb);
        let mean = (out[0] + out[1] + out[2]) / 3.0;
        assert!(out.iter().all(|v| (v / mean - 1.0).abs() < 1e-6), "{out:?}");
    }

    #[test]
    fn zero_white_balance_shift_is_identity() {
        assert!(close(&white_balance_matrix(0.0, 0.0), &IDENTITY, 1e-12));
    }

    #[test]
    fn positive_shift_warms_and_positive_tint_goes_magenta() {
        let warm = mul_vec(&white_balance_matrix(40.0, 0.0), [1.0, 1.0, 1.0]);
        assert!(warm[0] > warm[2], "{warm:?}");
        let cool = mul_vec(&white_balance_matrix(-40.0, 0.0), [1.0, 1.0, 1.0]);
        assert!(cool[2] > cool[0], "{cool:?}");
        let magenta = mul_vec(&white_balance_matrix(0.0, 50.0), [1.0, 1.0, 1.0]);
        assert!(magenta[1] < magenta[0] && magenta[1] < magenta[2], "{magenta:?}");
    }
}
