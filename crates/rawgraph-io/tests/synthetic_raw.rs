//! RAW tests without any real photo: a minimal uncompressed Bayer DNG is built in memory.
//! The sensor behaves like a camera whose primaries are sRGB (colour matrix = XYZ -> linear
//! sRGB), with neutral as-shot white balance, black level 64 and white level 1023.

use std::path::PathBuf;

use rawgraph_io::raw::{load_with_mode, Mode};
use rawgraph_io::*;

struct Tmp(PathBuf);
impl Tmp {
    fn new(name: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("rawgraph-io-raw-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const BLACK: u16 = 64;
const WHITE: u32 = 1023;

fn le16(v: &[u16]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn le32(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Builds an RGGB uncompressed 16-bit DNG from a mosaic.
fn make_dng(w: u32, h: u32, mosaic: &[u16], orientation: u16) -> Vec<u8> {
    make_dng_with(w, h, mosaic, orientation, &Extras::default())
}

/// Optional white-balance / exposure tags for the DNG builder.
#[derive(Default)]
struct Extras {
    /// AsShotNeutral (camera neutral, RATIONAL x/10000); default = (1, 1, 1).
    neutral: Option<[f64; 3]>,
    /// BaselineExposure in stops (SRATIONAL x/100); absent when `None`.
    baseline: Option<f64>,
    /// Second colour matrix + both calibration illuminants (21 = D65, 17 = A): (matrix for A).
    second_matrix: Option<[f64; 9]>,
    /// Leave out ColorMatrix1 and AsShotNeutral (a file without usable white-balance data).
    no_wb: bool,
}

fn rational_bytes(v: &[(i64, i64)], signed: bool) -> Vec<u8> {
    v.iter()
        .flat_map(|(n, d)| if signed { [(*n as i32).to_le_bytes(), (*d as i32).to_le_bytes()].concat() } else { [(*n as u32).to_le_bytes(), (*d as u32).to_le_bytes()].concat() })
        .collect()
}

fn make_dng_with(w: u32, h: u32, mosaic: &[u16], orientation: u16, extras: &Extras) -> Vec<u8> {
    assert_eq!(mosaic.len(), (w * h) as usize);
    let pixels = le16(mosaic);
    // XYZ(D65) -> linear sRGB, as SRATIONAL x/10000.
    let m = [3.2406f64, -1.5372, -0.4986, -0.9689, 1.8758, 0.0415, 0.0557, -0.2040, 1.0570];
    let color_matrix: Vec<u8> = m.iter().flat_map(|v| [((v * 10000.0).round() as i32).to_le_bytes(), 10000i32.to_le_bytes()].concat()).collect();
    let neutral: Vec<u8> = match extras.neutral {
        Some(n) => rational_bytes(&n.map(|v| ((v * 10000.0).round() as i64, 10000)), false),
        None => le32(&[1, 1, 1, 1, 1, 1]),
    };

    // (tag, type, count, data); sorted by tag. The strip offset is patched in below.
    let mut e: Vec<(u16, u16, u32, Vec<u8>)> = vec![
        (254, 4, 1, le32(&[0])),
        (256, 4, 1, le32(&[w])),
        (257, 4, 1, le32(&[h])),
        (258, 3, 1, le16(&[16])),
        (259, 3, 1, le16(&[1])),
        (262, 3, 1, le16(&[32803])),
        (271, 2, 8, b"TestCam\0".to_vec()),
        (272, 2, 11, b"Synthetic1\0".to_vec()),
        (273, 4, 1, le32(&[0])),
        (274, 3, 1, le16(&[orientation])),
        (277, 3, 1, le16(&[1])),
        (278, 4, 1, le32(&[h])),
        (279, 4, 1, le32(&[pixels.len() as u32])),
        (284, 3, 1, le16(&[1])),
        (33421, 3, 2, le16(&[2, 2])),
        (33422, 1, 4, vec![0, 1, 1, 2]),
        (50706, 1, 4, vec![1, 4, 0, 0]),
        (50708, 2, 19, b"TestCam Synthetic1\0".to_vec()),
        (50714, 3, 1, le16(&[BLACK])),
        (50717, 4, 1, le32(&[WHITE])),
        (50721, 10, 9, color_matrix),
        (50728, 5, 3, neutral),
        (50778, 3, 1, le16(&[21])),
    ];
    if let Some(b) = extras.baseline {
        e.push((50730, 10, 1, rational_bytes(&[((b * 100.0).round() as i64, 100)], true)));
    }
    if let Some(m_a) = extras.second_matrix {
        // ColorMatrix1 / illuminant 1 = A (the given matrix), ColorMatrix2 / illuminant 2 = D65 (sRGB).
        for t in e.iter_mut() {
            match t.0 {
                50721 => t.0 = 50722,
                50778 => t.0 = 50779,
                _ => {}
            }
        }
        e.push((50721, 10, 9, rational_bytes(&m_a.map(|v| ((v * 10000.0).round() as i64, 10000)), true)));
        e.push((50778, 3, 1, le16(&[17])));
    }
    if extras.no_wb {
        e.retain(|t| t.0 != 50721 && t.0 != 50722 && t.0 != 50728);
    }
    e.sort_by_key(|t| t.0);
    let ifd_len = 2 + 12 * e.len() + 4;
    let mut blob_off = 8 + ifd_len;
    let mut blobs: Vec<u8> = Vec::new();
    let mut offsets = Vec::new();
    for (_, _, _, d) in &e {
        if d.len() > 4 {
            offsets.push(Some(blob_off + blobs.len()));
            blobs.extend_from_slice(d);
            if blobs.len() % 2 == 1 {
                blobs.push(0);
            }
        } else {
            offsets.push(None);
        }
    }
    blob_off += blobs.len();
    let strip_offset = blob_off as u32;
    for (tag, _, _, d) in e.iter_mut() {
        if *tag == 273 {
            *d = le32(&[strip_offset]);
        }
    }
    let mut out = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
    out.extend_from_slice(&(e.len() as u16).to_le_bytes());
    for ((tag, typ, count, d), off) in e.iter().zip(&offsets) {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&typ.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        match off {
            Some(o) => out.extend_from_slice(&(*o as u32).to_le_bytes()),
            None => {
                let mut v = d.clone();
                v.resize(4, 0);
                out.extend_from_slice(&v);
            }
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&blobs);
    out.extend_from_slice(&pixels);
    out
}

/// Raw code value for a normalised level.
fn code(level: f32) -> u16 {
    (BLACK as f32 + level * (WHITE as f32 - BLACK as f32)).round() as u16
}

fn write(t: &Tmp, name: &str, w: u32, h: u32, mosaic: &[u16], orientation: u16) -> PathBuf {
    let p = t.0.join(name);
    std::fs::write(&p, make_dng(w, h, mosaic, orientation)).unwrap();
    p
}

#[test]
fn synthetic_dng_decodes_and_grey_stays_neutral_on_every_path() {
    let t = Tmp::new("grey");
    let (w, h) = (64u32, 64u32);
    let level = 0.18f32;
    let mosaic = vec![code(level); (w * h) as usize];
    let p = write(&t, "grey.dng", w, h, &mosaic, 1);
    let expect = (code(level) as f32 - BLACK as f32) / (WHITE as f32 - BLACK as f32);

    let info = read_capture_info(&p).unwrap();
    assert_eq!(info.make.as_deref(), Some("TestCam"));

    // Full resolution (full demosaic), proxy by binning (f = 2) and proxy through the reference path.
    let full = load(&p, None).unwrap();
    assert_eq!(full.kind, SourceKind::Raw);
    assert_eq!(full.full_size, (64, 64));
    let opts = LoadOptions { max_dim: Some(32), ..Default::default() };
    let (fast, _) = load_with_mode(&p, &opts, Mode::Auto).unwrap();
    let (slow, _) = load_with_mode(&p, &opts, Mode::FullDemosaic).unwrap();
    for (name, img) in [("full", &full.image), ("fast", &fast.image), ("reference", &slow.image)] {
        for px in img.data().as_chunks::<4>().0.iter() {
            for (c, v) in px.iter().take(3).enumerate() {
                assert!((v - expect).abs() < 2e-3, "{name}: channel {c} = {v}, expected {expect}");
            }
            assert_eq!(px[3], 1.0);
        }
    }
    assert_eq!((fast.image.width(), fast.image.height()), (32, 32));
    assert_eq!((fast.image.width(), fast.image.height()), (slow.image.width(), slow.image.height()));
    assert_eq!(fast.full_size, slow.full_size);
}

#[test]
fn fast_and_reference_proxy_sizes_agree_for_many_max_dims() {
    let t = Tmp::new("sizes");
    let (w, h) = (90u32, 70u32);
    let mosaic = vec![code(0.3); (w * h) as usize];
    let p = write(&t, "s.dng", w, h, &mosaic, 6); // orientation 6: oriented size is 70x90
    for max in [1u32, 8, 20, 29, 30, 31, 45, 64, 89, 90, 200] {
        let o = LoadOptions { max_dim: Some(max), ..Default::default() };
        let (a, _) = load_with_mode(&p, &o, Mode::Auto).unwrap();
        let (b, _) = load_with_mode(&p, &o, Mode::FullDemosaic).unwrap();
        assert_eq!((a.image.width(), a.image.height()), (b.image.width(), b.image.height()), "max_dim {max}");
        assert_eq!(a.full_size, (70, 90));
        assert_eq!(b.full_size, (70, 90));
        assert!(a.image.width().max(a.image.height()) <= max.max(1) || max < 2, "max_dim {max}");
    }
}

/// Left half: red photosites at the sensor clip level, green/blue dark; right half: mid grey.
fn clipped_left_mosaic(w: u32, h: u32) -> Vec<u16> {
    let mut m = vec![code(0.18); (w * h) as usize];
    for y in 0..h {
        for x in 0..w / 2 {
            m[(y * w + x) as usize] = if y % 2 == 0 && x % 2 == 0 { WHITE as u16 } else { code(0.06) };
        }
    }
    m
}

#[test]
fn neutralisation_option_changes_clipped_pixels_only() {
    let t = Tmp::new("clip");
    let (w, h) = (64u32, 64u32);
    let p = write(&t, "clip.dng", w, h, &clipped_left_mosaic(w, h), 1);
    for max_dim in [None, Some(32)] {
        let on = load_with(&p, &LoadOptions { max_dim, neutralize_clipped_highlights: true }).unwrap().image;
        let off = load_with(&p, &LoadOptions { max_dim, neutralize_clipped_highlights: false }).unwrap().image;
        let (iw, ih) = (on.width(), on.height());
        let scale = w / iw; // 1 or 2
        let (mut changed_left, mut changed_right) = (0, 0);
        for y in 0..ih {
            for x in 0..iw {
                let (a, b) = (on.pixel(x, y), off.pixel(x, y));
                let differs = (0..3).any(|c| (a[c] - b[c]).abs() > 1e-6);
                let xs = x * scale;
                if xs + 6 < w / 2 {
                    changed_left += differs as u32; // well inside the clipped half
                    if y > 4 && y + 4 < ih {
                        let spread = (a[0] - a[1]).abs().max((a[1] - a[2]).abs());
                        let cast = (b[0] - b[1]).abs();
                        assert!(cast > 0.05, "without neutralisation the cast must remain ({max_dim:?})");
                        assert!(spread < 0.2 * cast, "neutralised pixel must be (nearly) neutral, got {a:?} vs {b:?} ({max_dim:?})");
                    }
                }
                if xs > w / 2 + 6 {
                    changed_right += differs as u32; // untouched half
                }
            }
        }
        assert!(changed_left > 0, "clipped area must change ({max_dim:?})");
        assert_eq!(changed_right, 0, "unclipped pixels must not change ({max_dim:?})");
    }
}

#[test]
fn fast_and_reference_proxy_agree_on_a_gradient() {
    let t = Tmp::new("grad");
    let (w, h) = (96u32, 64u32);
    let mut m = vec![0u16; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            m[(y * w + x) as usize] = code(0.05 + 0.6 * x as f32 / w as f32 + 0.1 * y as f32 / h as f32);
        }
    }
    let p = write(&t, "g.dng", w, h, &m, 1);
    let o = LoadOptions { max_dim: Some(48), ..Default::default() };
    let (a, _) = load_with_mode(&p, &o, Mode::Auto).unwrap();
    let (b, _) = load_with_mode(&p, &o, Mode::FullDemosaic).unwrap();
    let n = (a.image.width() * a.image.height() * 3) as f64;
    let mut sum = 0.0f64;
    for (x, y) in a.image.data().as_chunks::<4>().0.iter().zip(b.image.data().as_chunks::<4>().0) {
        for c in 0..3 {
            sum += (x[c] - y[c]).abs() as f64;
        }
    }
    assert!(sum / n < 0.01, "mean abs difference {}", sum / n);
}

// ------------------------------------------------------------ white balance / baseline exposure

/// XYZ(D65) -> linear sRGB as used by the builder.
const SRGB_M: [f64; 9] = [3.2406, -1.5372, -0.4986, -0.9689, 1.8758, 0.0415, 0.0557, -0.2040, 1.0570];

fn write_extras(t: &Tmp, name: &str, extras: &Extras) -> PathBuf {
    let (w, h) = (32u32, 32u32);
    let p = t.0.join(name);
    std::fs::write(&p, make_dng_with(w, h, &vec![code(0.18); (w * h) as usize], 1, extras)).unwrap();
    p
}

#[test]
fn raw_flag_and_reference_white_gives_reference_temperature() {
    let t = Tmp::new("wb-ref");
    let p = write_extras(&t, "ref.dng", &Extras::default());
    let info = read_capture_info(&p).unwrap();
    assert_eq!(info.raw, Some(true));
    // Neutral (1,1,1) with an XYZ->sRGB matrix is the D65 white (6504 K, no tint).
    let k = info.as_shot_kelvin.expect("kelvin");
    let tint = info.as_shot_tint.expect("tint");
    assert!((k - 6504.0).abs() < 150.0, "kelvin {k}");
    // D65 itself lies slightly on the green side of the Planckian locus (Duv ~ +0.0032, i.e. tint ~ +9.6
    // with core's convention: positive tint = white point on the green side of the locus).
    assert!((tint - 9.6).abs() < 2.0, "tint {tint}");
    assert_eq!(info.baseline_exposure, None);
    assert_eq!(load(&p, None).unwrap().capture, info);
}

#[test]
fn warmer_neutral_gives_lower_kelvin_and_green_neutral_moves_tint() {
    let t = Tmp::new("wb-warm");
    let kelvin_of = |n: [f64; 3], name: &str| {
        let p = write_extras(&t, name, &Extras { neutral: Some(n), ..Default::default() });
        let info = read_capture_info(&p).unwrap();
        assert_eq!(load(&p, None).unwrap().capture, info);
        (info.as_shot_kelvin.expect("kelvin"), info.as_shot_tint.expect("tint"))
    };
    let (k_ref, t_ref) = kelvin_of([1.0, 1.0, 1.0], "a.dng");
    // Light that is redder than D65: the camera sees more red and less blue.
    let (k_warm, _) = kelvin_of([1.0, 0.8, 0.5], "b.dng");
    let (k_cool, _) = kelvin_of([0.7, 0.85, 1.0], "c.dng");
    assert!(k_warm < 5000.0 && k_warm < k_ref, "warm {k_warm} ref {k_ref}");
    assert!(k_cool > k_ref, "cool {k_cool} ref {k_ref}");
    // A white point further on the green side of the locus gives a larger tint (raw-converter
    // convention: the converter then adds magenta); a more magenta one gives a smaller tint.
    let (_, t_green) = kelvin_of([0.9, 1.0, 0.9], "d.dng");
    let (_, t_magenta) = kelvin_of([1.0, 0.85, 1.0], "e.dng");
    assert!(t_green > t_ref + 5.0, "green {t_green} ref {t_ref}");
    assert!(t_magenta < t_ref - 5.0, "magenta {t_magenta} ref {t_ref}");
}

#[test]
fn two_matrices_interpolate_between_their_illuminants() {
    // Matrix for illuminant A: rows scaled so that the A white (x .44757, y .40745) maps to (1,1,1).
    let a_xyz = [0.44757 / 0.40745, 1.0, (1.0 - 0.44757 - 0.40745) / 0.40745];
    let mut m_a = SRGB_M;
    for r in 0..3 {
        let s: f64 = (0..3).map(|c| SRGB_M[r * 3 + c] * a_xyz[c]).sum();
        for c in 0..3 {
            m_a[r * 3 + c] /= s;
        }
    }
    let t = Tmp::new("wb-two");
    let p = write_extras(&t, "two.dng", &Extras { second_matrix: Some(m_a), ..Default::default() });
    let info = read_capture_info(&p).unwrap();
    let k = info.as_shot_kelvin.expect("kelvin");
    assert!(k.is_finite() && (1667.0..=25000.0).contains(&k), "kelvin {k}");
    // Neutral (1,1,1) is the A white under the A matrix and ~D65 under the D65 matrix, so the
    // consistent estimate lies between the two reference temperatures.
    assert!(k > 2800.0 && k < 6600.0, "kelvin {k}");
    assert_eq!(load(&p, None).unwrap().capture, info);
}

#[test]
fn baseline_exposure_is_read_and_absent_gives_none() {
    let t = Tmp::new("baseline");
    for (name, b) in [("plus.dng", 0.75), ("minus.dng", -1.5)] {
        let p = write_extras(&t, name, &Extras { baseline: Some(b), ..Default::default() });
        let info = read_capture_info(&p).unwrap();
        assert!((info.baseline_exposure.expect("baseline") as f64 - b).abs() < 1e-6, "{name}");
        assert_eq!(load(&p, None).unwrap().capture, info);
    }
    let p = write_extras(&t, "none.dng", &Extras::default());
    assert_eq!(read_capture_info(&p).unwrap().baseline_exposure, None);
}

#[test]
fn dng_without_white_balance_data_reports_none() {
    let t = Tmp::new("wb-none");
    let p = write_extras(&t, "nowb.dng", &Extras { no_wb: true, ..Default::default() });
    let info = read_capture_info(&p).unwrap();
    assert_eq!(info.raw, Some(true));
    assert_eq!((info.as_shot_kelvin, info.as_shot_tint), (None, None));
}
