//! ICC profiles: the sRGB profile that exports embed, and a reader for the matrix/TRC RGB
//! profiles that rendered input files may carry.
//!
//! **Output.** [`srgb_icc_profile`] is generated in code (ICC v2.1, matrix/TRC, D50 PCS): the
//! colorants are the Rec.709 primaries Bradford-adapted from D65 to the ICC D50, the three curves
//! are the sRGB curve sampled at 1024 points (one shared table), `wtpt` is D50.
//!
//! **Input.** [`parse`] understands RGB profiles that consist of `rXYZ/gXYZ/bXYZ` and
//! `rTRC/gTRC/bTRC` (curve types `curv` as identity, gamma or sampled table, and `para` types 0–4).
//! Anything else (LUT-only, CMYK, grey, truncated, unknown curve type) is reported as unsupported
//! and the caller falls back to sRGB. Nothing in here panics on profile content.

use std::sync::OnceLock;

use rawgraph_core::color::{self, adaptation, mul, mul_vec, xy_to_xyz, Mat3, OutputSpace, Transfer, ACES_WHITE_XY, AP1_TO_XYZ};

use crate::InputProfile;

/// The ICC profile connection space white (D50) as stored in profile headers.
pub(crate) const ICC_D50: [f64; 3] = [0.9642, 1.0, 0.8249];

// ---------------------------------------------------------------- writing

fn s15(v: f64) -> [u8; 4] {
    ((v * 65536.0).round() as i32).to_be_bytes()
}

fn xyz_tag(v: [f64; 3]) -> Vec<u8> {
    let mut d = b"XYZ \0\0\0\0".to_vec();
    for c in v {
        d.extend_from_slice(&s15(c));
    }
    d
}

/// `textDescriptionType` (ICC v2): ASCII only, empty Unicode and ScriptCode parts.
fn desc_tag(text: &str) -> Vec<u8> {
    let ascii: Vec<u8> = text.bytes().filter(|b| (0x20..0x7f).contains(b)).collect();
    let mut d = b"desc\0\0\0\0".to_vec();
    d.extend_from_slice(&(ascii.len() as u32 + 1).to_be_bytes());
    d.extend_from_slice(&ascii);
    d.push(0);
    d.extend_from_slice(&[0; 8]); // Unicode language code + count
    d.extend_from_slice(&[0; 3]); // ScriptCode code + count
    d.extend_from_slice(&[0; 67]);
    d
}

fn text_tag(text: &str) -> Vec<u8> {
    let mut d = b"text\0\0\0\0".to_vec();
    d.extend_from_slice(text.as_bytes());
    d.push(0);
    d
}

/// Transfer curve of a generated profile.
pub(crate) enum TrcSpec {
    /// The sRGB curve as a 1024-entry table.
    Srgb,
    /// A pure power law (`curv` with one `u8Fixed8` entry; the value is rounded to 1/256).
    Gamma(f64),
    /// A pure power law as a 1024-entry table, for exponents that are not a multiple of 1/256
    /// (the profile is v2.1, which has no parametric curves).
    GammaTable(f64),
}

fn curve_tag(spec: &TrcSpec) -> Vec<u8> {
    let mut d = b"curv\0\0\0\0".to_vec();
    match spec {
        TrcSpec::Srgb => {
            const N: usize = 1024;
            d.extend_from_slice(&(N as u32).to_be_bytes());
            for i in 0..N {
                let x = i as f64 / (N - 1) as f64;
                let y = if x <= 0.04045 { x / 12.92 } else { ((x + 0.055) / 1.055).powf(2.4) };
                d.extend_from_slice(&((y * 65535.0).round() as u16).to_be_bytes());
            }
        }
        TrcSpec::Gamma(g) => {
            d.extend_from_slice(&1u32.to_be_bytes());
            d.extend_from_slice(&((g * 256.0).round().clamp(0.0, 65535.0) as u16).to_be_bytes());
            d.extend_from_slice(&[0, 0]);
        }
        TrcSpec::GammaTable(g) => {
            const N: usize = 1024;
            d.extend_from_slice(&(N as u32).to_be_bytes());
            for i in 0..N {
                let y = (i as f64 / (N - 1) as f64).powf(*g);
                d.extend_from_slice(&((y * 65535.0).round() as u16).to_be_bytes());
            }
        }
    }
    d
}

/// RGB -> XYZ(D50) colorants of an RGB space given by xy primaries and white (Bradford to ICC D50).
pub(crate) fn colorants_d50(primaries: [(f64, f64); 3], white: (f64, f64)) -> Mat3 {
    let p = [xy_to_xyz(primaries[0]), xy_to_xyz(primaries[1]), xy_to_xyz(primaries[2])];
    let cols: Mat3 = [[p[0][0], p[1][0], p[2][0]], [p[0][1], p[1][1], p[2][1]], [p[0][2], p[1][2], p[2][2]]];
    let s = mul_vec(&color::invert(&cols), xy_to_xyz(white));
    let m: Mat3 = [
        [cols[0][0] * s[0], cols[0][1] * s[1], cols[0][2] * s[2]],
        [cols[1][0] * s[0], cols[1][1] * s[1], cols[1][2] * s[2]],
        [cols[2][0] * s[0], cols[2][1] * s[1], cols[2][2] * s[2]],
    ];
    mul(&adaptation(xy_to_xyz(white), ICC_D50), &m)
}

/// Builds a v2.1 matrix/TRC RGB profile. `colorants` are the columns R, G, B in XYZ(D50).
pub(crate) fn build_matrix_profile(description: &str, colorants: &Mat3, trc: &TrcSpec) -> Vec<u8> {
    let col = |c: usize| [colorants[0][c], colorants[1][c], colorants[2][c]];
    let curve = curve_tag(trc);
    let mut tags: Vec<([u8; 4], Vec<u8>)> = vec![
        (*b"bTRC", curve.clone()),
        (*b"bXYZ", xyz_tag(col(2))),
        (*b"cprt", text_tag("No copyright, use freely")),
        (*b"desc", desc_tag(description)),
        (*b"gTRC", curve.clone()),
        (*b"gXYZ", xyz_tag(col(1))),
        (*b"rTRC", curve),
        (*b"rXYZ", xyz_tag(col(0))),
        (*b"wtpt", xyz_tag(ICC_D50)),
    ];
    tags.sort_by_key(|t| t.0);

    let table_len = 4 + 12 * tags.len();
    let mut body: Vec<u8> = Vec::new();
    let mut placed: Vec<(usize, usize)> = Vec::new(); // (tag index, offset) of identical data already written
    let mut entries: Vec<([u8; 4], u32, u32)> = Vec::new();
    for (i, (sig, data)) in tags.iter().enumerate() {
        let reuse = placed.iter().find(|(j, _)| tags[*j].1 == *data).map(|(_, off)| *off);
        let off = match reuse {
            Some(off) => off,
            None => {
                let off = 128 + table_len + body.len();
                body.extend_from_slice(data);
                while !body.len().is_multiple_of(4) {
                    body.push(0);
                }
                placed.push((i, off));
                off
            }
        };
        entries.push((*sig, off as u32, data.len() as u32));
    }

    let total = 128 + table_len + body.len();
    let mut p = vec![0u8; 128];
    p[0..4].copy_from_slice(&(total as u32).to_be_bytes());
    p[8..12].copy_from_slice(&0x0210_0000u32.to_be_bytes());
    p[12..16].copy_from_slice(b"mntr");
    p[16..20].copy_from_slice(b"RGB ");
    p[20..24].copy_from_slice(b"XYZ ");
    // Creation date: fixed, so the bytes are reproducible.
    for (i, v) in [2026u16, 11, 15, 0, 0, 0].iter().enumerate() {
        p[24 + i * 2..26 + i * 2].copy_from_slice(&v.to_be_bytes());
    }
    p[36..40].copy_from_slice(b"acsp");
    for (i, c) in ICC_D50.iter().enumerate() {
        p[68 + i * 4..72 + i * 4].copy_from_slice(&s15(*c));
    }
    p.extend_from_slice(&(tags.len() as u32).to_be_bytes());
    for (sig, off, size) in entries {
        p.extend_from_slice(&sig);
        p.extend_from_slice(&off.to_be_bytes());
        p.extend_from_slice(&size.to_be_bytes());
    }
    p.extend_from_slice(&body);
    p
}

/// The ICC profile that exports embed for sRGB: a small (about 3 KB) ICC v2.1 matrix/TRC profile
/// for sRGB IEC 61966-2.1 (Rec.709 primaries Bradford-adapted to the ICC D50 PCS, sRGB curve, D50
/// white). Same bytes as `icc_profile(OutputSpace::Srgb)`.
pub fn srgb_icc_profile() -> &'static [u8] {
    icc_profile(OutputSpace::Srgb)
}

/// The ICC profile of an export colour space: ICC v2.1 matrix/TRC, colorants from
/// [`OutputSpace::primaries`] (white D65, Bradford-adapted to the ICC D50 PCS), the transfer curve
/// from [`OutputSpace::transfer`] (sRGB curve: 1024-entry table; Adobe RGB 563/256: one-entry
/// `curv`; Rec. 2020 gamma 2.4: 1024-entry table), `wtpt` = D50. Generated once per space.
pub fn icc_profile(space: OutputSpace) -> &'static [u8] {
    static PROFILES: [OnceLock<Vec<u8>>; 4] = [const { OnceLock::new() }; 4];
    let (slot, name) = match space {
        OutputSpace::Srgb => (0, "sRGB IEC61966-2.1 (RawGraph)"),
        OutputSpace::DisplayP3 => (1, "Display P3 (RawGraph)"),
        OutputSpace::AdobeRgb => (2, "Adobe RGB (1998) (RawGraph)"),
        OutputSpace::Rec2020 => (3, "Rec. 2020 (RawGraph)"),
    };
    PROFILES[slot].get_or_init(|| {
        let colorants = colorants_d50(space.primaries(), color::D65_XY);
        let trc = match space.transfer() {
            Transfer::Srgb => TrcSpec::Srgb,
            Transfer::Gamma(g) if ((g * 256.0).round() / 256.0 - g).abs() < 1e-9 => TrcSpec::Gamma(g),
            Transfer::Gamma(g) => TrcSpec::GammaTable(g),
        };
        build_matrix_profile(name, &colorants, &trc)
    })
}

// ---------------------------------------------------------------- reading

/// Transfer curve of an input profile.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Curve {
    Identity,
    Gamma(f64),
    /// Normalised samples over 0..1.
    Table(Vec<f64>),
    /// `para` with function type 0..=4 and parameters g, a, b, c, d, e, f.
    Para(u16, [f64; 7]),
}

impl Curve {
    /// Encoded value (0..1) -> linear.
    pub(crate) fn eval(&self, x: f64) -> f64 {
        let x = if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) };
        let y = match self {
            Curve::Identity => x,
            Curve::Gamma(g) => x.powf(*g),
            Curve::Table(t) => {
                let pos = x * (t.len() - 1) as f64;
                let i = (pos.floor() as usize).min(t.len() - 1);
                let j = (i + 1).min(t.len() - 1);
                t[i] + (t[j] - t[i]) * (pos - i as f64)
            }
            Curve::Para(kind, p) => {
                let [g, a, b, c, d, e, f] = *p;
                let pw = |v: f64| if v <= 0.0 { 0.0 } else { v.powf(g) };
                match kind {
                    0 => pw(x),
                    1 => {
                        if a != 0.0 && x >= -b / a {
                            pw(a * x + b)
                        } else {
                            0.0
                        }
                    }
                    2 => {
                        if a != 0.0 && x >= -b / a {
                            pw(a * x + b) + c
                        } else {
                            c
                        }
                    }
                    3 => {
                        if x >= d {
                            pw(a * x + b)
                        } else {
                            c * x
                        }
                    }
                    _ => {
                        if x >= d {
                            pw(a * x + b) + e
                        } else {
                            c * x + f
                        }
                    }
                }
            }
        };
        if y.is_finite() {
            y
        } else {
            0.0
        }
    }
}

/// A usable matrix/TRC RGB profile.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MatrixProfile {
    pub description: String,
    /// RGB -> XYZ(D50): the colorant XYZ values as columns.
    pub colorants: Mat3,
    pub curves: [Curve; 3],
}

/// Outcome of reading a profile.
#[allow(clippy::large_enum_variant)] // short-lived value, boxed only to please the size lint
pub(crate) enum Parsed {
    Matrix(MatrixProfile),
    /// The profile cannot be used; the text says why.
    Unsupported(String),
}

fn be32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(off..off.checked_add(4)?)?.try_into().ok()?))
}

fn be16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(off..off.checked_add(2)?)?.try_into().ok()?))
}

fn s15_at(b: &[u8], off: usize) -> Option<f64> {
    Some(be32(b, off)? as i32 as f64 / 65536.0)
}

fn tag_data<'a>(profile: &'a [u8], sig: &[u8; 4]) -> Option<&'a [u8]> {
    let n = be32(profile, 128)? as usize;
    if n > 1000 {
        return None;
    }
    for i in 0..n {
        let e = 132 + i * 12;
        if profile.get(e..e + 4)? == sig {
            let off = be32(profile, e + 4)? as usize;
            let size = be32(profile, e + 8)? as usize;
            return profile.get(off..off.checked_add(size)?);
        }
    }
    None
}

fn has_tag(profile: &[u8], sig: &[u8; 4]) -> bool {
    tag_data(profile, sig).is_some()
}

fn read_xyz(profile: &[u8], sig: &[u8; 4]) -> Option<[f64; 3]> {
    let d = tag_data(profile, sig)?;
    if d.get(0..4)? != b"XYZ " {
        return None;
    }
    Some([s15_at(d, 8)?, s15_at(d, 12)?, s15_at(d, 16)?])
}

fn read_curve(profile: &[u8], sig: &[u8; 4]) -> Option<Curve> {
    let d = tag_data(profile, sig)?;
    match d.get(0..4)? {
        b"curv" => {
            let n = be32(d, 8)? as usize;
            match n {
                0 => Some(Curve::Identity),
                1 => Some(Curve::Gamma(be16(d, 12)? as f64 / 256.0)),
                _ => {
                    if n > 65536 {
                        return None;
                    }
                    let mut t = Vec::with_capacity(n);
                    for i in 0..n {
                        t.push(be16(d, 12 + i * 2)? as f64 / 65535.0);
                    }
                    Some(Curve::Table(t))
                }
            }
        }
        b"para" => {
            let kind = be16(d, 8)?;
            let count = match kind {
                0 => 1,
                1 => 3,
                2 => 4,
                3 => 5,
                4 => 7,
                _ => return None,
            };
            let mut p = [0.0f64; 7];
            for (i, v) in p.iter_mut().enumerate().take(count) {
                *v = s15_at(d, 12 + i * 4)?;
            }
            Some(Curve::Para(kind, p))
        }
        _ => None,
    }
}

fn read_description(profile: &[u8]) -> String {
    let fallback = || "embedded ICC profile".to_string();
    let Some(d) = tag_data(profile, b"desc") else { return fallback() };
    let text = match d.get(0..4) {
        Some(b"desc") => (|| {
            let n = be32(d, 8)? as usize;
            let s = d.get(12..12usize.checked_add(n)?)?;
            Some(String::from_utf8_lossy(s).trim_matches('\0').to_string())
        })(),
        Some(b"mluc") => (|| {
            let n = be32(d, 8)?;
            if n == 0 {
                return None;
            }
            let (len, off) = (be32(d, 20)? as usize, be32(d, 24)? as usize);
            let s = d.get(off..off.checked_add(len)?)?;
            let units: Vec<u16> = s.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
            Some(String::from_utf16_lossy(&units))
        })(),
        _ => None,
    };
    match text.map(|t| t.trim().to_string()) {
        Some(t) if !t.is_empty() => t,
        _ => fallback(),
    }
}

/// Reads a profile. Never panics.
pub(crate) fn parse(profile: &[u8]) -> Parsed {
    let unsupported = |why: &str| Parsed::Unsupported(why.to_string());
    if profile.len() < 132 || profile.get(36..40) != Some(b"acsp") {
        return unsupported("not an ICC profile");
    }
    let space = profile.get(16..20).unwrap_or_default();
    if space != b"RGB " {
        return Parsed::Unsupported(format!("{} colour space profile", String::from_utf8_lossy(space).trim()));
    }
    let (Some(r), Some(g), Some(b)) = (read_xyz(profile, b"rXYZ"), read_xyz(profile, b"gXYZ"), read_xyz(profile, b"bXYZ")) else {
        return unsupported(if has_tag(profile, b"A2B0") { "LUT-based RGB profile" } else { "no matrix colorants" });
    };
    let (Some(rc), Some(gc), Some(bc)) = (read_curve(profile, b"rTRC"), read_curve(profile, b"gTRC"), read_curve(profile, b"bTRC")) else {
        return unsupported("unreadable tone curves");
    };
    let colorants: Mat3 = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
    let det = color::invert(&colorants);
    if colorants.iter().flatten().any(|v| !v.is_finite()) || det.iter().flatten().any(|v| !v.is_finite()) {
        return unsupported("degenerate colorants");
    }
    Parsed::Matrix(MatrixProfile { description: read_description(profile), colorants, curves: [rc, gc, bc] })
}

/// sRGB colorants (D50) and curve, for recognising sRGB profiles.
fn srgb_reference() -> Mat3 {
    colorants_d50([(0.64, 0.33), (0.30, 0.60), (0.15, 0.06)], color::D65_XY)
}

/// True if the profile is sRGB to within 1.5e-3 in colorants and in linear value of the curves.
fn is_srgb(p: &MatrixProfile) -> bool {
    let reference = srgb_reference();
    for r in 0..3 {
        for c in 0..3 {
            if (reference[r][c] - p.colorants[r][c]).abs() > 1.5e-3 {
                return false;
            }
        }
    }
    p.curves.iter().all(|curve| {
        (0..=64).all(|i| {
            let x = i as f64 / 64.0;
            let want = if x <= 0.04045 { x / 12.92 } else { ((x + 0.055) / 1.055).powf(2.4) };
            (curve.eval(x) - want).abs() <= 1.5e-3
        })
    })
}

/// Conversion of one input profile to ACEScg.
pub(crate) struct InputTransform {
    pub curves: [Curve; 3],
    /// Linear profile RGB -> ACEScg.
    pub matrix: [[f32; 3]; 3],
}

impl InputTransform {
    /// Linear-light lookup table for one channel with `n` code values (256 or 65536).
    pub(crate) fn lut(&self, channel: usize, n: usize) -> Vec<f32> {
        (0..n).map(|v| self.curves[channel].eval(v as f64 / (n - 1) as f64) as f32).collect()
    }
}

/// Decides how a file's embedded profile (if any) is treated. `None` as transform means "plain
/// sRGB", the pre-existing behaviour.
pub(crate) fn resolve_input(icc: Option<&[u8]>) -> (Option<InputTransform>, InputProfile) {
    let Some(bytes) = icc.filter(|b| !b.is_empty()) else { return (None, InputProfile::Untagged) };
    match parse(bytes) {
        Parsed::Unsupported(why) => (None, InputProfile::UnsupportedTreatedAsSrgb(why)),
        Parsed::Matrix(p) => {
            if is_srgb(&p) {
                return (None, InputProfile::Srgb);
            }
            let to_xyz = mul(&adaptation(ICC_D50, xy_to_xyz(ACES_WHITE_XY)), &p.colorants);
            let m = mul(&color::invert(&AP1_TO_XYZ), &to_xyz);
            (Some(InputTransform { curves: p.curves, matrix: color::to_f32(&m) }), InputProfile::Matrix(p.description))
        }
    }
}

/// Linear light (0..=1, 8192 steps) -> sRGB code value, shared by the thumbnail conversion.
fn srgb_code_table() -> &'static [u8] {
    static TABLE: OnceLock<Vec<u8>> = OnceLock::new();
    TABLE.get_or_init(|| (0..8192).map(|i| (color::srgb_oetf(i as f32 / 8191.0).clamp(0.0, 1.0) * 255.0 + 0.5) as u8).collect())
}

/// Converts RGBA8 pixels coded in the embedded profile `icc` to sRGB codes, in place (the
/// thumbnail and provisional-preview counterpart of the full decode: same curves, same primaries
/// and Bradford adaptation as [`resolve_input`], same ACEScg working space in between). Returns
/// whether anything was changed: no profile, an sRGB profile and unusable profiles leave the
/// pixels alone (as the full decode reads them as sRGB). Out-of-gamut colours are clamped.
pub(crate) fn thumbnail_to_srgb(icc: Option<&[u8]>, rgba: &mut [u8]) -> bool {
    let (Some(xf), _) = resolve_input(icc) else { return false };
    let lut = [xf.lut(0, 256), xf.lut(1, 256), xf.lut(2, 256)];
    let out = color::to_f32(&color::acescg_to_rec709());
    let mut m = [[0.0f32; 3]; 3];
    for (r, row) in m.iter_mut().enumerate() {
        for (c, v) in row.iter_mut().enumerate() {
            *v = (0..3).map(|k| out[r][k] * xf.matrix[k][c]).sum();
        }
    }
    let table = srgb_code_table();
    let code = |lin: f32| table[(lin.clamp(0.0, 1.0) * 8191.0 + 0.5) as usize];
    for p in rgba.as_chunks_mut::<4>().0 {
        let (r, g, b) = (lut[0][p[0] as usize], lut[1][p[1] as usize], lut[2][p[2] as usize]);
        p[0] = code(m[0][0] * r + m[0][1] * g + m[0][2] * b);
        p[1] = code(m[1][0] * r + m[1][1] * g + m[1][2] * b);
        p[2] = code(m[2][0] * r + m[2][1] * g + m[2][2] * b);
    }
    true
}

/// The ICC profile of the first JPEG in `file` that has one (APP2 `ICC_PROFILE` marker segments),
/// for embedded RAW previews: the decoder hands over pixels only, so the container bytes are
/// searched. Which JPEG the profile belongs to is not checked: camera previews that carry a
/// profile are tagged consistently.
pub(crate) fn find_jpeg_icc(file: &[u8]) -> Option<Vec<u8>> {
    const SIG: &[u8; 12] = b"ICC_PROFILE ";
    let finder = memchr::memmem::Finder::new(SIG);
    let mut from = 0usize;
    while let Some(rel) = finder.find(file.get(from..)?) {
        let sig_at = from + rel;
        from = sig_at + SIG.len();
        // Segment: FF E2, length (2, includes itself), signature, sequence, count, data.
        let Some(start) = sig_at.checked_sub(4) else { continue };
        if file.get(start..start + 2) != Some(&[0xFF, 0xE2]) {
            continue;
        }
        let mut at = start;
        let mut profile = Vec::new();
        let mut expect = 1u8;
        let mut total = 0u8;
        while let Some(h) = file.get(at..at + 18) {
            let len = usize::from(u16::from_be_bytes([h[2], h[3]]));
            if h[0] != 0xFF || h[1] != 0xE2 || &h[4..16] != SIG || h[16] != expect || len < 16 {
                break;
            }
            total = h[17];
            let Some(data) = file.get(at + 18..at + 2 + len) else { break };
            profile.extend_from_slice(data);
            if expect == total {
                return Some(profile);
            }
            expect += 1;
            at += 2 + len;
        }
        let _ = total;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Published sRGB D50-adapted colorants (Bradford), from the sRGB IEC61966-2.1 profile / Lindbloom.
    const PUBLISHED: [[f64; 3]; 3] = [[0.4360747, 0.3850649, 0.1430804], [0.2225045, 0.7168786, 0.0606169], [0.0139322, 0.0971045, 0.7141733]];

    #[test]
    fn profile_structure_is_valid() {
        let p = srgb_icc_profile();
        assert!(p.len() < 4096, "profile should be small, is {} bytes", p.len());
        assert_eq!(be32(p, 0), Some(p.len() as u32), "header size field");
        assert_eq!(&p[36..40], b"acsp");
        assert_eq!(&p[12..16], b"mntr");
        assert_eq!(&p[16..20], b"RGB ");
        assert_eq!(&p[20..24], b"XYZ ");
        assert_eq!(be32(p, 8), Some(0x0210_0000));
        let n = be32(p, 128).expect("tag count") as usize;
        let mut seen = Vec::new();
        for i in 0..n {
            let e = 132 + i * 12;
            let sig = &p[e..e + 4];
            let (off, size) = (be32(p, e + 4).expect("offset") as usize, be32(p, e + 8).expect("size") as usize);
            assert!(off >= 132 + n * 12 && off % 4 == 0 && off + size <= p.len(), "tag {sig:?} outside the file");
            seen.push(String::from_utf8_lossy(sig).to_string());
        }
        for required in ["desc", "cprt", "wtpt", "rXYZ", "gXYZ", "bXYZ", "rTRC", "gTRC", "bTRC"] {
            assert!(seen.iter().any(|s| s == required), "missing tag {required}");
        }
        // The XYZ values of white and the colorants add up: R + G + B = D50.
        let sum: Vec<f64> = (0..3)
            .map(|k| {
                [b"rXYZ", b"gXYZ", b"bXYZ"].iter().map(|s| read_xyz(p, s).expect("colorant")[k]).sum::<f64>()
            })
            .collect();
        for k in 0..3 {
            assert!((sum[k] - ICC_D50[k]).abs() < 2e-4, "colorants must add up to D50: {sum:?}");
        }
        assert_eq!(read_xyz(p, b"wtpt").map(|w| w.map(|v| (v * 1e4).round() / 1e4)), Some([0.9642, 1.0, 0.8249]));
    }

    #[test]
    fn profile_matches_published_primaries() {
        let p = srgb_icc_profile();
        let Parsed::Matrix(m) = parse(p) else { panic!("own profile must parse as a matrix profile") };
        // (1,0,0), (0,1,0), (0,0,1) -> the columns.
        for (k, unit) in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]].iter().enumerate() {
            let xyz = mul_vec(&m.colorants, *unit);
            for r in 0..3 {
                assert!((xyz[r] - PUBLISHED[r][k]).abs() < 1e-3, "primary {k} component {r}: {} vs {}", xyz[r], PUBLISHED[r][k]);
            }
        }
        assert_eq!(m.description, "sRGB IEC61966-2.1 (RawGraph)");
        // The curve is the sRGB curve.
        for i in 0..=255 {
            let x = i as f64 / 255.0;
            let want = if x <= 0.04045 { x / 12.92 } else { ((x + 0.055) / 1.055).powf(2.4) };
            for c in &m.curves {
                assert!((c.eval(x) - want).abs() < 2e-4, "curve at {x}");
            }
        }
        assert!(matches!(resolve_input(Some(p)), (None, InputProfile::Srgb)));
    }

    #[test]
    fn broken_and_foreign_profiles_are_unsupported() {
        let p = srgb_icc_profile().to_vec();
        assert!(matches!(resolve_input(Some(&p[..100])).1, InputProfile::UnsupportedTreatedAsSrgb(_)));
        assert!(matches!(resolve_input(Some(&[0u8; 500])).1, InputProfile::UnsupportedTreatedAsSrgb(_)));
        let mut grey = p.clone();
        grey[16..20].copy_from_slice(b"GRAY");
        assert!(matches!(resolve_input(Some(&grey)).1, InputProfile::UnsupportedTreatedAsSrgb(_)));
        let mut cmyk = p.clone();
        cmyk[16..20].copy_from_slice(b"CMYK");
        assert!(matches!(resolve_input(Some(&cmyk)).1, InputProfile::UnsupportedTreatedAsSrgb(_)));
        // Colorants renamed: no matrix left (as in a LUT-only profile).
        let mut lut = p.clone();
        let n = be32(&lut, 128).expect("count") as usize;
        for i in 0..n {
            let e = 132 + i * 12;
            if &lut[e..e + 4] == b"rXYZ" {
                lut[e..e + 4].copy_from_slice(b"A2B0");
            }
        }
        assert!(matches!(resolve_input(Some(&lut)).1, InputProfile::UnsupportedTreatedAsSrgb(_)));
        // Offsets pointing outside the file never panic.
        let mut bad = p.clone();
        bad[132 + 4..132 + 8].copy_from_slice(&u32::MAX.to_be_bytes());
        let _ = resolve_input(Some(&bad));
        assert!(matches!(resolve_input(None), (None, InputProfile::Untagged)));
        assert!(matches!(resolve_input(Some(&[])), (None, InputProfile::Untagged)));
    }

    #[test]
    fn parametric_curves_evaluate() {
        // Type 3 with the sRGB parameters equals the sRGB curve.
        let c = Curve::Para(3, [2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045, 0.0, 0.0]);
        for i in 0..=100 {
            let x = i as f64 / 100.0;
            let want = if x <= 0.04045 { x / 12.92 } else { ((x + 0.055) / 1.055).powf(2.4) };
            assert!((c.eval(x) - want).abs() < 1e-9);
        }
        assert!((Curve::Para(0, [2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]).eval(0.5) - 0.25).abs() < 1e-12);
        assert!((Curve::Gamma(2.0).eval(0.5) - 0.25).abs() < 1e-12);
    }

    const ALL_SPACES: [OutputSpace; 4] = [OutputSpace::Srgb, OutputSpace::DisplayP3, OutputSpace::AdobeRgb, OutputSpace::Rec2020];

    #[test]
    fn srgb_profile_is_unchanged_by_the_generalisation() {
        // The bytes the old hard-coded builder produced.
        let colorants = colorants_d50([(0.64, 0.33), (0.30, 0.60), (0.15, 0.06)], color::D65_XY);
        let old = build_matrix_profile("sRGB IEC61966-2.1 (RawGraph)", &colorants, &TrcSpec::Srgb);
        assert_eq!(srgb_icc_profile(), &old[..]);
        assert_eq!(icc_profile(OutputSpace::Srgb), &old[..]);
    }

    #[test]
    fn every_output_profile_is_a_valid_matrix_profile() {
        for space in ALL_SPACES {
            let p = icc_profile(space);
            assert!(p.len() < 8192, "{space:?}: {} bytes", p.len());
            assert_eq!(be32(p, 0), Some(p.len() as u32), "{space:?}: header size field");
            assert_eq!((&p[36..40], &p[12..16], &p[16..20], &p[20..24]), (&b"acsp"[..], &b"mntr"[..], &b"RGB "[..], &b"XYZ "[..]));
            let n = be32(p, 128).expect("tag count") as usize;
            let mut seen = Vec::new();
            for i in 0..n {
                let e = 132 + i * 12;
                let (off, size) = (be32(p, e + 4).expect("offset") as usize, be32(p, e + 8).expect("size") as usize);
                assert!(off >= 132 + n * 12 && off % 4 == 0 && off + size <= p.len(), "{space:?}: tag {i} outside the file");
                seen.push(String::from_utf8_lossy(&p[e..e + 4]).to_string());
            }
            for required in ["desc", "cprt", "wtpt", "rXYZ", "gXYZ", "bXYZ", "rTRC", "gTRC", "bTRC"] {
                assert!(seen.iter().any(|s| s == required), "{space:?}: missing tag {required}");
            }
            // R + G + B colorants are the D50 white.
            for (k, white) in ICC_D50.iter().enumerate() {
                let sum: f64 = [b"rXYZ", b"gXYZ", b"bXYZ"].iter().map(|s| read_xyz(p, s).expect("colorant")[k]).sum();
                assert!((sum - white).abs() < 1e-3, "{space:?}: colorants add up to {sum} for component {k}");
            }
            let wt = read_xyz(p, b"wtpt").expect("white");
            assert!((0..3).all(|k| (wt[k] - ICC_D50[k]).abs() < 1e-4), "{space:?}: wtpt");
            // Description text.
            let Parsed::Matrix(m) = parse(p) else { panic!("{space:?}: own profile must parse") };
            let want = match space {
                OutputSpace::Srgb => "sRGB IEC61966-2.1 (RawGraph)",
                OutputSpace::DisplayP3 => "Display P3 (RawGraph)",
                OutputSpace::AdobeRgb => "Adobe RGB (1998) (RawGraph)",
                OutputSpace::Rec2020 => "Rec. 2020 (RawGraph)",
            };
            assert_eq!(m.description, want);
            // The colorants are the primaries (white D65, Bradford to D50) and the curve is the space's transfer.
            let expect = colorants_d50(space.primaries(), color::D65_XY);
            for r in 0..3 {
                for c in 0..3 {
                    assert!((m.colorants[r][c] - expect[r][c]).abs() < 2e-5, "{space:?}: colorant [{r}][{c}]");
                }
            }
            for i in 0..=255 {
                let x = i as f64 / 255.0;
                let want = match space.transfer() {
                    Transfer::Srgb => {
                        if x <= 0.04045 {
                            x / 12.92
                        } else {
                            ((x + 0.055) / 1.055).powf(2.4)
                        }
                    }
                    Transfer::Gamma(g) => x.powf(g),
                };
                for c in &m.curves {
                    assert!((c.eval(x) - want).abs() < 2e-4, "{space:?}: curve at {x}: {} vs {want}", c.eval(x));
                }
            }
        }
    }

    #[test]
    fn well_known_colorants_of_the_published_profiles() {
        // Published D50-adapted red/green/blue columns (Lindbloom / ICC reference profiles), first row = X.
        let published: [(OutputSpace, [f64; 3]); 3] = [
            (OutputSpace::DisplayP3, [0.5151, 0.2920, 0.1571]),   // X of R, G, B (Display P3, D50)
            (OutputSpace::AdobeRgb, [0.6097, 0.2053, 0.1492]),    // Adobe RGB (1998)
            (OutputSpace::Rec2020, [0.6734, 0.1656, 0.1251]),     // Rec. 2020
        ];
        for (space, xs) in published {
            let Parsed::Matrix(m) = parse(icc_profile(space)) else { panic!("parse") };
            for k in 0..3 {
                assert!((m.colorants[0][k] - xs[k]).abs() < 1.5e-3, "{space:?}: X of primary {k}: {} vs {}", m.colorants[0][k], xs[k]);
            }
        }
        // The Adobe RGB curve is the one-entry gamma 563/256; Rec. 2020 is a table.
        let adobe = tag_data(icc_profile(OutputSpace::AdobeRgb), b"rTRC").expect("curve");
        assert_eq!((be32(adobe, 8), be16(adobe, 12)), (Some(1), Some(563)));
        let rec = tag_data(icc_profile(OutputSpace::Rec2020), b"rTRC").expect("curve");
        assert_eq!(be32(rec, 8), Some(1024));
    }

    #[test]
    fn output_profiles_are_read_back_as_the_same_colour_space() {
        // An exported file re-imported through `resolve_input` is recognised (sRGB) or decoded as a matrix profile.
        assert!(matches!(resolve_input(Some(icc_profile(OutputSpace::Srgb))), (None, InputProfile::Srgb)));
        for space in [OutputSpace::DisplayP3, OutputSpace::AdobeRgb, OutputSpace::Rec2020] {
            assert!(matches!(resolve_input(Some(icc_profile(space))), (Some(_), InputProfile::Matrix(_))), "{space:?}");
        }
    }
}

#[cfg(test)]
mod input_tests {
    use super::*;
    use crate::rendered;
    use image::codecs::jpeg::JpegEncoder;
    use image::codecs::png::PngEncoder;
    use image::codecs::tiff::TiffEncoder;
    use image::{ExtendedColorType, ImageEncoder};
    use std::path::{Path, PathBuf};

    const ADOBE: [(f64, f64); 3] = [(0.64, 0.33), (0.21, 0.71), (0.15, 0.06)];
    const P3: [(f64, f64); 3] = [(0.68, 0.32), (0.265, 0.69), (0.15, 0.06)];
    const SRGB: [(f64, f64); 3] = [(0.64, 0.33), (0.30, 0.60), (0.15, 0.06)];

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Dir {
            let p = std::env::temp_dir().join(format!("rawgraph-io-icc-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("temp dir");
            Dir(p)
        }
        fn file(&self, n: &str) -> PathBuf {
            self.0.join(n)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn adobe_profile() -> Vec<u8> {
        build_matrix_profile("Adobe RGB (1998)", &colorants_d50(ADOBE, color::D65_XY), &TrcSpec::Gamma(2.199_218_75))
    }

    fn p3_profile() -> Vec<u8> {
        build_matrix_profile("Display P3", &colorants_d50(P3, color::D65_XY), &TrcSpec::Srgb)
    }

    /// Independent reference: linear RGB of a space with the given primaries (white D65) -> ACEScg,
    /// computed in one Bradford step D65 -> ACES white without going through the ICC D50.
    fn expected(primaries: [(f64, f64); 3], lin: [f64; 3]) -> [f64; 3] {
        let p = [xy_to_xyz(primaries[0]), xy_to_xyz(primaries[1]), xy_to_xyz(primaries[2])];
        let cols: Mat3 = [[p[0][0], p[1][0], p[2][0]], [p[0][1], p[1][1], p[2][1]], [p[0][2], p[1][2], p[2][2]]];
        let s = mul_vec(&color::invert(&cols), xy_to_xyz(color::D65_XY));
        let m: Mat3 = [
            [cols[0][0] * s[0], cols[0][1] * s[1], cols[0][2] * s[2]],
            [cols[1][0] * s[0], cols[1][1] * s[1], cols[1][2] * s[2]],
            [cols[2][0] * s[0], cols[2][1] * s[1], cols[2][2] * s[2]],
        ];
        let cat = adaptation(xy_to_xyz(color::D65_XY), xy_to_xyz(ACES_WHITE_XY));
        mul_vec(&mul(&color::invert(&AP1_TO_XYZ), &mul(&cat, &m)), lin)
    }

    fn adobe_eotf(v: u8) -> f64 {
        (v as f64 / 255.0).powf(2.199_218_75)
    }

    fn srgb_eotf(v: u8) -> f64 {
        let x = v as f64 / 255.0;
        if x <= 0.04045 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    }

    /// 4x1 image: pure R, G, B and a mixed colour.
    const PIXELS: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [200, 100, 50]];

    fn write_png(path: &Path, icc: Option<&[u8]>, pixels: &[[u8; 3]]) {
        let raw: Vec<u8> = pixels.iter().flatten().copied().collect();
        let file = std::fs::File::create(path).expect("create");
        let mut enc = PngEncoder::new(file);
        if let Some(p) = icc {
            enc.set_icc_profile(p.to_vec()).expect("icc");
        }
        enc.write_image(&raw, pixels.len() as u32, 1, ExtendedColorType::Rgb8).expect("png");
    }

    fn check(loaded: &crate::Loaded, primaries: [(f64, f64); 3], eotf: impl Fn(u8) -> f64, tol: f64) {
        for (i, px) in PIXELS.iter().enumerate() {
            let want = expected(primaries, [eotf(px[0]), eotf(px[1]), eotf(px[2])]);
            let got = &loaded.image.data()[i * 4..i * 4 + 3];
            for c in 0..3 {
                assert!((got[c] as f64 - want[c]).abs() < tol, "pixel {i} channel {c}: got {} want {}", got[c], want[c]);
            }
        }
    }

    #[test]
    fn adobe_rgb_png_lands_on_published_acescg_values() {
        let d = Dir::new("adobe-png");
        let p = d.file("a.png");
        write_png(&p, Some(&adobe_profile()), &PIXELS);
        let l = rendered::load(&p, None).expect("load");
        assert_eq!(l.profile, InputProfile::Matrix("Adobe RGB (1998)".into()));
        check(&l, ADOBE, adobe_eotf, 2e-3);
        // Treating the file as sRGB would give a clearly different green.
        let srgb_like = expected(SRGB, [0.0, adobe_eotf(255), 0.0]);
        assert!((l.image.data()[4 + 1] as f64 - srgb_like[1]).abs() > 0.02);
    }

    #[test]
    fn display_p3_png_lands_on_published_acescg_values() {
        let d = Dir::new("p3-png");
        let p = d.file("p3.png");
        write_png(&p, Some(&p3_profile()), &PIXELS);
        let l = rendered::load(&p, None).expect("load");
        assert_eq!(l.profile, InputProfile::Matrix("Display P3".into()));
        check(&l, P3, srgb_eotf, 2e-3);
    }

    #[test]
    fn adobe_rgb_16bit_tiff_and_jpeg() {
        let d = Dir::new("adobe-tif");
        let p = d.file("a.tif");
        let raw16: Vec<u8> = PIXELS.iter().flatten().flat_map(|v| (*v as u16 * 257).to_ne_bytes()).collect();
        {
            let mut enc = TiffEncoder::new(std::fs::File::create(&p).expect("create"));
            enc.set_icc_profile(adobe_profile()).expect("icc");
            enc.write_image(&raw16, 4, 1, ExtendedColorType::Rgb16).expect("tiff");
        }
        let l = rendered::load(&p, None).expect("load");
        assert!(matches!(l.profile, InputProfile::Matrix(_)));
        check(&l, ADOBE, adobe_eotf, 2e-3);

        // JPEG is lossy: a flat colour at top quality.
        let j = d.file("a.jpg");
        let flat: Vec<u8> = (0..64).flat_map(|_| [200u8, 100, 50]).collect();
        {
            let mut enc = JpegEncoder::new_with_quality(std::fs::File::create(&j).expect("create"), 100);
            enc.set_icc_profile(adobe_profile()).expect("icc");
            enc.write_image(&flat, 8, 8, ExtendedColorType::Rgb8).expect("jpeg");
        }
        let l = rendered::load(&j, None).expect("load");
        assert!(matches!(l.profile, InputProfile::Matrix(_)));
        let want = expected(ADOBE, [adobe_eotf(200), adobe_eotf(100), adobe_eotf(50)]);
        let got = &l.image.data()[..3];
        for c in 0..3 {
            assert!((got[c] as f64 - want[c]).abs() < 0.02, "jpeg channel {c}: got {} want {}", got[c], want[c]);
        }
    }

    #[test]
    fn tagged_srgb_equals_untagged_and_foreign_profiles_fall_back() {
        let d = Dir::new("srgb");
        let (plain, tagged, other, cmyk) = (d.file("plain.png"), d.file("tagged.png"), d.file("other.png"), d.file("cmyk.png"));
        write_png(&plain, None, &PIXELS);
        write_png(&tagged, Some(srgb_icc_profile()), &PIXELS);
        // Same sRGB data under another description.
        let mut renamed = build_matrix_profile("sRGB built-in", &colorants_d50(SRGB, color::D65_XY), &TrcSpec::Srgb);
        write_png(&other, Some(&renamed), &PIXELS);
        renamed[16..20].copy_from_slice(b"CMYK");
        write_png(&cmyk, Some(&renamed), &PIXELS);

        let a = rendered::load(&plain, None).expect("plain");
        assert_eq!(a.profile, InputProfile::Untagged);
        for f in [&tagged, &other] {
            let b = rendered::load(f, None).expect("tagged");
            assert_eq!(b.profile, InputProfile::Srgb);
            for (x, y) in a.image.data().iter().zip(b.image.data()) {
                assert!((x - y).abs() <= 2e-3, "tagged sRGB must equal untagged: {x} vs {y}");
            }
        }
        let c = rendered::load(&cmyk, None).expect("cmyk");
        assert!(matches!(c.profile, InputProfile::UnsupportedTreatedAsSrgb(_)));
        assert_eq!(a.image.data(), c.image.data());
    }

    #[test]
    fn gamma_22_profile_with_srgb_primaries_uses_its_own_curve() {
        let d = Dir::new("g22");
        let p = d.file("g.png");
        let prof = build_matrix_profile("Gamma 2.2", &colorants_d50(SRGB, color::D65_XY), &TrcSpec::Gamma(2.2));
        write_png(&p, Some(&prof), &[[128, 128, 128]]);
        let l = rendered::load(&p, None).expect("load");
        assert!(matches!(l.profile, InputProfile::Matrix(_)));
        let lin = (128.0f64 / 255.0).powf(2.199_218_75); // 2.2 stored as u8Fixed8
        let want = expected(SRGB, [lin; 3]);
        assert!((l.image.data()[1] as f64 - want[1]).abs() < 2e-3);
    }
}

#[cfg(test)]
mod jpeg_icc_tests {
    use super::find_jpeg_icc;

    fn segment(seq: u8, count: u8, data: &[u8]) -> Vec<u8> {
        let mut s = vec![0xFF, 0xE2];
        s.extend_from_slice(&((data.len() + 16) as u16).to_be_bytes());
        s.extend_from_slice(b"ICC_PROFILE\0");
        s.extend_from_slice(&[seq, count]);
        s.extend_from_slice(data);
        s
    }

    #[test]
    fn profile_split_over_segments_is_reassembled_after_other_bytes() {
        let mut file = vec![0u8; 100];
        file.extend_from_slice(&[0xFF, 0xD8]);
        file.extend(segment(1, 2, &[1, 2, 3]));
        file.extend(segment(2, 2, &[4, 5]));
        file.extend_from_slice(&[0xFF, 0xDB, 0, 4, 9, 9]);
        assert_eq!(find_jpeg_icc(&file), Some(vec![1, 2, 3, 4, 5]));
    }

    #[test]
    #[ignore = "diagnostic timing, run explicitly with --ignored --nocapture"]
    fn scan_cost_of_a_file_without_a_profile() {
        let file: Vec<u8> = (0..60_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        let t = std::time::Instant::now();
        assert_eq!(find_jpeg_icc(&file), None);
        println!("scan of 60 MB: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    }

    #[test]
    fn broken_or_missing_chains_give_none() {
        assert_eq!(find_jpeg_icc(&[]), None);
        assert_eq!(find_jpeg_icc(b"ICC_PROFILE\0"), None);
        let mut gap = segment(1, 3, &[1]);
        gap.extend(segment(3, 3, &[2]));
        assert_eq!(find_jpeg_icc(&gap), None, "segment 2 is missing");
        let mut cut = segment(1, 1, &[1, 2, 3, 4]);
        cut.truncate(cut.len() - 2);
        assert_eq!(find_jpeg_icc(&cut), None);
    }
}
