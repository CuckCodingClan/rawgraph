//! RAW / DNG input.
//!
//! Decoding of the container, the sensor data and the camera metadata is done by the pure-Rust
//! [`rawler`] crate (LGPL-2.1, from the dnglab project). Everything after that is done here, so
//! the colour pipeline is explicit and does not pass through a clipped sRGB intermediate:
//!
//! 1. **Linearise / normalise**: per CFA position `v = (raw - black) / (white - black)` with the
//!    black and white levels that `rawler` reports for the file (per channel / per 2x2 position).
//!    Sensor clipping therefore maps to exactly 1.0. Values *below* black (noise) and above white
//!    are **not** clipped; they stay as the sensor delivered them.
//! 2. **Demosaic** the active sensor area: PPG (patterned pixel grouping) for Bayer sensors,
//!    bilinear for X-Trans. Files that are already linear RGB (`cpp == 3`, e.g. Canon sRAW or
//!    linear DNG) skip this step.
//! 3. **As-shot white balance**: multiply by the camera's as-shot coefficients (from the file's
//!    maker note or the DNG `AsShotNeutral`), normalised so green is 1.0. If the file has none,
//!    coefficients of 1.0 are used (the matrix below then renders camera neutral as D65 white).
//! 4. **Camera colour matrix**: the file's `XYZ(D65) -> camera` matrix (DNG `ColorMatrix` or the
//!    `rawler` camera database; a matrix for another illuminant is Bradford-adapted to D65), with
//!    its rows scaled so D65 white maps to camera (1,1,1). It is inverted to camera -> XYZ(D65).
//! 5. **XYZ -> ACEScg**: Bradford adaptation D65 -> ACES white, then the inverse of `AP1_TO_XYZ`.
//!    Steps 4–5 are one 3x3 matrix applied in f32 after step 3.
//!
//!    **Sensor-clip neutralisation** (between 3 and 4): when a camera channel is at the sensor
//!    clip level, the true colour of that pixel is unknowable and a plain white balance would
//!    give it a strong colour cast (typically magenta, because green clips at a lower white-balanced
//!    value than red/blue). Such pixels are blended, smoothly from 90 % to 100 % of the clip
//!    level, towards a neutral pixel whose three channels equal the brightest white-balanced
//!    channel. In white-balanced camera space no channel is lowered or clipped (after the colour matrix an
//!    individual ACEScg channel can still move down a little); it only removes the colour of
//!    blown pixels. It can be switched off with `LoadOptions::neutralize_clipped_highlights`.
//! 6. **Crop** to the camera's default crop (DNG `DefaultCrop` / `rawler` camera definition).
//! 7. **Orientation** from the EXIF orientation tag is applied last.
//!
//! Not done, on purpose: tone curve, gamma, sharpening, noise reduction,
//! real highlight reconstruction (detail in clipped areas is not recovered), lens corrections,
//! DNG opcode lists, dual-illuminant matrix interpolation, `ForwardMatrix`/`ProfileHueSatMap`
//! (those are look/profile decisions of later graph nodes).
//!
//! **Proxy decodes (`max_dim`).** The reduction factor is `f = ceil(long_edge / max_dim)` of the
//! cropped image, exactly as `ImageBuf::downscale_box`, so sizes never depend on the path taken.
//! For Bayer sensors with `f >= 2` no demosaic runs: every output pixel is the per-colour mean of
//! the `f` x `f` block of CFA samples (for `f == 2` this is the classic superpixel: R, mean of the
//! two G, B), with black/white normalisation fused in; white balance, clip handling and the matrix
//! then run on the small image. X-Trans, linear files and `f == 1` use the full demosaic and a box
//! filter (the clip handling is then applied per full-resolution pixel before averaging).
//! Differences between the two paths are small (mean ~1 % of image brightness) and concentrated
//! at single-pixel highlights and hard edges, where binning and demosaic+average sample differently.

use std::path::Path;

use rawgraph_core::color::{self, Mat3};
use rawgraph_core::{CaptureInfo, ImageBuf};
use rawler::decoders::{RawDecodeParams, RawMetadata};
use rawler::imgop::sensor::bayer::ppg::PPGDemosaic;
use rawler::imgop::sensor::xtrans::bilinear::XTransBilinearDemosaic;
use rawler::imgop::sensor::{Demosaic, SensorType};
use rawler::imgop::xyz::Illuminant;
use rawler::imgop::{Dim2, Point, Rect};
use rawler::pixarray::{Color2D, PixF32};
use rawler::rawimage::{RawImage, RawImageData, RawPhotometricInterpretation};
use rawler::rawsource::RawSource;
use rayon::prelude::*;

use crate::orient::{orient_rgba, oriented_size, swaps_axes};
use crate::rendered::guard;
use crate::{IoError, LoadOptions, Loaded, SourceKind};

fn rawler_err(e: rawler::RawlerError) -> IoError {
    IoError::Decode(e.to_string())
}

fn open_source(path: &Path) -> Result<RawSource, IoError> {
    let meta = std::fs::metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => IoError::NotFound(path.to_path_buf()),
        _ => IoError::Io(format!("{}: {e}", path.display())),
    })?;
    if meta.len() == 0 {
        return Err(IoError::Decode("file is empty".into()));
    }
    RawSource::new(path).map_err(|e| IoError::Io(format!("{}: {e}", path.display())))
}

// ---------------------------------------------------------------- metadata

fn positive(r: Option<rawler::formats::tiff::Rational>) -> Option<f32> {
    let r = r?;
    (r.d != 0 && r.n != 0).then(|| r.n as f32 / r.d as f32)
}

/// Calibration illuminants usable for the as-shot white point: xy and correlated colour
/// temperature in kelvin (used only to order the matrices and as interpolation end points).
fn calibration_illuminant(i: &Illuminant) -> Option<((f64, f64), f64)> {
    let xy = illuminant_xy(i)?;
    let kelvin = match i {
        Illuminant::D65 => 6504.0,
        Illuminant::A | Illuminant::Tungsten | Illuminant::IsoStudioTungsten => 2856.0,
        Illuminant::B => 4874.0,
        Illuminant::C => 6774.0,
        Illuminant::D50 => 5003.0,
        Illuminant::D55 | Illuminant::Daylight | Illuminant::FineWeather => 5503.0,
        Illuminant::D75 => 7504.0,
        _ => return None,
    };
    Some((xy, kelvin))
}

fn mat3_from_flat(flat: &[f32]) -> Option<Mat3> {
    if flat.len() < 9 || !flat.len().is_multiple_of(3) || flat.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let mut m = [[0.0f64; 3]; 3];
    for (r, row) in m.iter_mut().enumerate() {
        for (c, v) in row.iter_mut().enumerate() {
            *v = flat[r * 3 + c] as f64;
        }
    }
    (det3(&m).abs() > 1e-9).then_some(m)
}

/// xy chromaticity of camera-neutral `n` under the matrix `xyz2cam` (XYZ -> camera).
fn neutral_to_xy(xyz2cam: &Mat3, n: [f64; 3]) -> Option<(f64, f64)> {
    if det3(xyz2cam).abs() < 1e-9 {
        return None;
    }
    let xyz = color::mul_vec(&color::invert(xyz2cam), n);
    let s = xyz[0] + xyz[1] + xyz[2];
    if !s.is_finite() || s.abs() < 1e-12 {
        return None;
    }
    let (x, y) = (xyz[0] / s, xyz[1] / s);
    (x.is_finite() && y.is_finite() && x > 0.0 && y > 0.0).then_some((x, y))
}

/// As-shot white point (kelvin, tint) of a RAW file: the camera neutral (1 / as-shot white-balance
/// multipliers) converted to XYZ with the file's colour matrix, then to kelvin / tint.
///
/// With matrices for two calibration illuminants the procedure of the DNG specification is used:
/// the matrix is interpolated by inverse colour temperature between the lowest and the highest
/// calibration illuminant, the white point is estimated, and both steps repeat until stable (at
/// most 30 rounds). With a single matrix that matrix is used as is. The matrices are taken
/// unscaled (as stored), in the DNG convention where XYZ is the real XYZ of the scene
/// illuminant; row-scaling to D65 as done by the pixel pipeline would corrupt the neutral.
/// White-balance inputs of a decoded RAW header (the part of `RawImage` the white point needs).
struct WbSource {
    wb_coeffs: [f32; 4],
    color_matrix: std::collections::HashMap<Illuminant, Vec<f32>>,
}

impl WbSource {
    fn of(raw: &RawImage) -> WbSource {
        WbSource { wb_coeffs: raw.wb_coeffs, color_matrix: raw.color_matrix.clone() }
    }
}

fn as_shot_white(src: &WbSource) -> Option<(f32, f32)> {
    let c = src.wb_coeffs;
    if !(c[0].is_finite() && c[1].is_finite() && c[2].is_finite() && c[0] > 0.0 && c[1] > 0.0 && c[2] > 0.0) {
        return None;
    }
    let n = [(c[1] / c[0]) as f64, 1.0, (c[1] / c[2]) as f64];
    let mats: Vec<(f64, Mat3)> = src
        .color_matrix
        .iter()
        .filter_map(|(i, flat)| Some((calibration_illuminant(i)?.1, mat3_from_flat(flat)?)))
        .collect();
    white_from_neutral(n, mats)
}

/// Core of [`as_shot_white`]: camera neutral `n` and (illuminant kelvin, XYZ -> camera) matrices.
fn white_from_neutral(n: [f64; 3], mut mats: Vec<(f64, Mat3)>) -> Option<(f32, f32)> {
    if !n.iter().all(|v| v.is_finite() && *v > 0.0) {
        return None;
    }
    // Sorted by temperature; one entry per distinct temperature.
    mats.sort_by(|a, b| a.0.total_cmp(&b.0));
    mats.dedup_by(|a, b| a.0 == b.0);
    let (lo, hi) = (*mats.first()?, *mats.last()?);

    let mut xy = if mats.len() == 1 { neutral_to_xy(&lo.1, n)? } else { neutral_to_xy(&hi.1, n)? };
    if mats.len() > 1 {
        for _ in 0..30 {
            let (kelvin, _) = color::xy_to_kelvin_tint(xy);
            let kelvin = kelvin.clamp(lo.0, hi.0);
            // Weight of the low-temperature matrix, linear in mired.
            let g = ((1.0 / kelvin - 1.0 / hi.0) / (1.0 / lo.0 - 1.0 / hi.0)).clamp(0.0, 1.0);
            let mut m = [[0.0f64; 3]; 3];
            for (r, row) in m.iter_mut().enumerate() {
                for (k, v) in row.iter_mut().enumerate() {
                    *v = g * lo.1[r][k] + (1.0 - g) * hi.1[r][k];
                }
            }
            let next = neutral_to_xy(&m, n)?;
            let done = (next.0 - xy.0).abs() < 1e-7 && (next.1 - xy.1).abs() < 1e-7;
            xy = next;
            if done {
                break;
            }
        }
    }
    let (kelvin, tint) = color::xy_to_kelvin_tint(xy);
    (kelvin.is_finite() && tint.is_finite() && (1667.0..=25000.0).contains(&kelvin)).then_some((kelvin as f32, tint as f32))
}

/// What a DNG carries in its first IFD about white balance and exposure, read with a few small
/// reads (no pixel data, about 1 ms) so that `read_capture_info` stays cheap.
struct DngIfd0 {
    /// `BaselineExposure` in stops.
    baseline: Option<f32>,
    /// `AsShotNeutral` (camera neutral).
    neutral: Option<[f64; 3]>,
    /// (calibration illuminant kelvin, XYZ -> camera) for `ColorMatrix1/2`.
    matrices: Vec<(f64, Mat3)>,
}

fn read_u16(le: bool, b: &[u8]) -> u16 {
    let a = [b[0], b[1]];
    if le { u16::from_le_bytes(a) } else { u16::from_be_bytes(a) }
}

fn read_u32(le: bool, b: &[u8]) -> u32 {
    let a = [b[0], b[1], b[2], b[3]];
    if le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
}

/// Numeric values of one TIFF entry (SHORT, LONG, RATIONAL, SRATIONAL), at most 64 of them.
fn tiff_values(f: &mut std::fs::File, le: bool, entry: &[u8; 12]) -> Option<Vec<f64>> {
    use std::io::{Read, Seek, SeekFrom};
    let (typ, count) = (read_u16(le, &entry[2..4]), read_u32(le, &entry[4..8]) as usize);
    let size = match typ {
        3 => 2,
        4 => 4,
        5 | 10 => 8,
        _ => return None,
    };
    if count == 0 || count > 64 {
        return None;
    }
    let mut buf = vec![0u8; size * count];
    if buf.len() <= 4 {
        let len = buf.len();
        buf.copy_from_slice(&entry[8..8 + len]);
    } else {
        f.seek(SeekFrom::Start(read_u32(le, &entry[8..12]) as u64)).ok()?;
        f.read_exact(&mut buf).ok()?;
    }
    let out: Vec<f64> = buf
        .chunks_exact(size)
        .map(|c| match typ {
            3 => read_u16(le, c) as f64,
            4 => read_u32(le, c) as f64,
            5 => {
                let d = read_u32(le, &c[4..8]);
                if d == 0 { f64::NAN } else { read_u32(le, &c[0..4]) as f64 / d as f64 }
            }
            _ => {
                let (n, d) = (read_u32(le, &c[0..4]) as i32, read_u32(le, &c[4..8]) as i32);
                if d == 0 { f64::NAN } else { n as f64 / d as f64 }
            }
        })
        .collect();
    out.iter().all(|v| v.is_finite()).then_some(out)
}

/// `None` for non-DNG files and anything malformed.
fn read_dng_ifd0(path: &Path) -> Option<DngIfd0> {
    use std::io::{Read, Seek, SeekFrom};
    const DNG_VERSION: u16 = 50706;
    const COLOR_MATRIX_1: u16 = 50721;
    const COLOR_MATRIX_2: u16 = 50722;
    const AS_SHOT_NEUTRAL: u16 = 50728;
    const BASELINE_EXPOSURE: u16 = 50730;
    const CALIBRATION_ILLUMINANT_1: u16 = 50778;
    const CALIBRATION_ILLUMINANT_2: u16 = 50779;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 8];
    f.read_exact(&mut head).ok()?;
    let le = match &head[..4] {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };
    f.seek(SeekFrom::Start(read_u32(le, &head[4..8]) as u64)).ok()?;
    let mut cnt = [0u8; 2];
    f.read_exact(&mut cnt).ok()?;
    let n = read_u16(le, &cnt) as usize;
    if n == 0 || n > 4096 {
        return None;
    }
    let mut table = vec![0u8; n * 12];
    f.read_exact(&mut table).ok()?;
    let entries: Vec<[u8; 12]> = table.as_chunks::<12>().0.to_vec();
    let find = |tag: u16| entries.iter().find(|e| read_u16(le, &e[0..2]) == tag);
    find(DNG_VERSION)?;

    let mut out = DngIfd0 { baseline: None, neutral: None, matrices: Vec::new() };
    if let Some(v) = find(BASELINE_EXPOSURE).and_then(|e| tiff_values(&mut f, le, e)) {
        out.baseline = v.first().copied().filter(|b| b.abs() <= 16.0).map(|b| b as f32);
    }
    if let Some(v) = find(AS_SHOT_NEUTRAL).and_then(|e| tiff_values(&mut f, le, e)) {
        if v.len() == 3 {
            out.neutral = Some([v[0], v[1], v[2]]);
        }
    }
    for (matrix_tag, illuminant_tag) in [(COLOR_MATRIX_1, CALIBRATION_ILLUMINANT_1), (COLOR_MATRIX_2, CALIBRATION_ILLUMINANT_2)] {
        let (Some(me), Some(ie)) = (find(matrix_tag), find(illuminant_tag)) else { continue };
        let (Some(m), Some(i)) = (tiff_values(&mut f, le, me), tiff_values(&mut f, le, ie)) else { continue };
        let Some(kelvin) = Illuminant::try_from(i[0] as u16).ok().and_then(|i| calibration_illuminant(&i)).map(|c| c.1) else {
            continue;
        };
        let flat: Vec<f32> = m.iter().map(|v| *v as f32).collect();
        if flat.len() == 9 {
            if let Some(mat) = mat3_from_flat(&flat) {
                out.matrices.push((kelvin, mat));
            }
        }
    }
    Some(out)
}

/// The entry `tag` of the TIFF IFD at `offset`, if the IFD is readable and has it.
fn ifd_entry(f: &mut std::fs::File, le: bool, offset: u64, tag: u16) -> Option<[u8; 12]> {
    use std::io::{Read, Seek, SeekFrom};
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut cnt = [0u8; 2];
    f.read_exact(&mut cnt).ok()?;
    let n = read_u16(le, &cnt) as usize;
    if n == 0 || n > 4096 {
        return None;
    }
    let mut table = vec![0u8; n * 12];
    f.read_exact(&mut table).ok()?;
    table.as_chunks::<12>().0.iter().find(|e| read_u16(le, &e[0..2]) == tag).copied()
}

/// `FocalLengthIn35mmFilm` (EXIF 0xA405) of a TIFF-based file (DNG, ARW, NEF, CR2, ...): two small
/// reads of IFD0 and the Exif IFD, no pixel data. The RAW decoder's EXIF block does not carry it.
/// `None` for other containers, a missing tag and anything malformed.
fn read_focal_35mm(path: &Path) -> Option<f32> {
    use std::io::Read;
    const EXIF_IFD_POINTER: u16 = 0x8769;
    const FOCAL_LENGTH_35: u16 = 0xA405;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 8];
    f.read_exact(&mut head).ok()?;
    let le = match &head[..4] {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };
    let pointer = ifd_entry(&mut f, le, read_u32(le, &head[4..8]) as u64, EXIF_IFD_POINTER)?;
    let entry = ifd_entry(&mut f, le, read_u32(le, &pointer[8..12]) as u64, FOCAL_LENGTH_35)?;
    let v = tiff_values(&mut f, le, &entry)?.first().copied()?;
    (1.0..=2000.0).contains(&v).then_some(v as f32)
}

// ---------------------------------------------------------------- Sony ARW white balance

/// Bounds-checked reader over a byte slice (the memory-mapped file or a decrypted block).
struct Tiff<'a> {
    buf: &'a [u8],
    le: bool,
}

/// IFD entry: (tag, type, count, position of the 4-byte value field).
type Entry = (u16, u16, u32, usize);

impl<'a> Tiff<'a> {
    fn new(buf: &'a [u8]) -> Option<Self> {
        match buf.get(..4)? {
            [b'I', b'I', 42, 0] => Some(Tiff { buf, le: true }),
            [b'M', b'M', 0, 42] => Some(Tiff { buf, le: false }),
            _ => None,
        }
    }

    fn u16(&self, off: usize) -> Option<u16> {
        let b: [u8; 2] = self.buf.get(off..off.checked_add(2)?)?.try_into().ok()?;
        Some(if self.le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }

    fn u32(&self, off: usize) -> Option<u32> {
        let b: [u8; 4] = self.buf.get(off..off.checked_add(4)?)?.try_into().ok()?;
        Some(if self.le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    fn entries(&self, off: usize) -> Option<Vec<Entry>> {
        let n = self.u16(off)? as usize;
        if n == 0 || n > 2048 {
            return None;
        }
        (0..n)
            .map(|i| {
                let e = off.checked_add(2 + i * 12)?;
                Some((self.u16(e)?, self.u16(e + 2)?, self.u32(e + 4)?, e + 8))
            })
            .collect()
    }

    /// `i`-th integer value of an entry (BYTE/SHORT/LONG/UNDEFINED). Values that do not fit the
    /// 4-byte value field are read through the offset stored there, minus `shift`.
    fn uint(&self, e: &Entry, i: usize, shift: usize) -> Option<u32> {
        let size = match e.1 {
            1 | 7 => 1,
            3 | 8 => 2,
            4 | 9 => 4,
            _ => return None,
        };
        if i >= e.2 as usize {
            return None;
        }
        let pos = if size * e.2 as usize <= 4 { e.3 } else { (self.u32(e.3)? as usize).checked_sub(shift)? };
        let at = pos.checked_add(i * size)?;
        match size {
            1 => self.buf.get(at).map(|v| *v as u32),
            2 => self.u16(at).map(|v| v as u32),
            _ => self.u32(at),
        }
    }
}

fn find(entries: &[Entry], tag: u16) -> Option<&Entry> {
    entries.iter().find(|e| e.0 == tag)
}

/// Port of the Sony SR2 decryption (dcraw `sony_decrypt`) over whole 32-bit words.
fn sony_decrypt(words: &[u8], key: u32) -> Vec<u8> {
    let mut pad = [0u32; 128];
    let mut k = key;
    for p in pad.iter_mut().take(4) {
        k = k.wrapping_mul(48828125).wrapping_add(1);
        *p = k;
    }
    pad[3] = (pad[3] << 1) | ((pad[0] ^ pad[2]) >> 31);
    for p in 4..127 {
        pad[p] = ((pad[p - 4] ^ pad[p - 2]) << 1) | ((pad[p - 3] ^ pad[p - 1]) >> 31);
    }
    for p in pad.iter_mut().take(127) {
        *p = u32::from_be(*p);
    }
    let mut out = Vec::with_capacity(words.len());
    for (i, w) in words.as_chunks::<4>().0.iter().enumerate() {
        let p = i + 127;
        pad[p & 127] = pad[(p + 1) & 127] ^ pad[(p + 1 + 64) & 127];
        let v = u32::from_le_bytes([w[0], w[1], w[2], w[3]]) ^ pad[p & 127];
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// White balance and colour matrices of a Sony ARW without decoding: the values the RAW decoder's
/// header pass produces (as-shot coefficients from the encrypted SR2 block, matrices from the camera
/// database), but only the few kilobytes of the SR2 block are read; the decoder copies the whole
/// file first. `None` for anything unexpected (other makers, sRAW with three samples per pixel,
/// unknown cameras, malformed data): the caller then falls back to the decoder.
fn arw_wb_source(buf: &[u8]) -> Option<WbSource> {
    const DNG_PRIVATE: u16 = 0xC634;
    let t = Tiff::new(buf)?;
    if !t.le {
        return None;
    }
    let ifd0 = t.entries(t.u32(4)? as usize)?;
    // IFD0 and its SubIFDs (tag 330).
    let mut ifds = vec![ifd0.clone()];
    if let Some(sub) = find(&ifd0, 330) {
        for i in 0..(sub.2 as usize).min(8) {
            if let Some(entries) = t.uint(sub, i, 0).and_then(|o| t.entries(o as usize)) {
                ifds.push(entries);
            }
        }
    }
    let raw = ifds.iter().find(|e| find(e, 273).is_some())?;
    let compression = find(raw, 259).and_then(|e| t.uint(e, 0, 0))?;
    if find(raw, 277).and_then(|e| t.uint(e, 0, 0)).unwrap_or(1) != 1 {
        return None; // sRAW: the decoder reports neutral coefficients there
    }
    let mode = if compression == 32766 { "arw6" } else { "" };
    let text = |tag: u16| -> Option<String> {
        let e = find(&ifd0, tag)?;
        let off = if e.2 <= 4 { e.3 } else { t.u32(e.3)? as usize };
        let s = buf.get(off..off.checked_add(e.2 as usize)?)?;
        Some(String::from_utf8_lossy(s).trim_end_matches(['\0', ' ']).to_string())
    };
    let (make, model) = (text(271)?, text(272)?);
    let camera = rawler::global_loader().get_cameras().get(&(make, model, mode.to_string()))?;

    let private = ifds.iter().find_map(|e| find(e, DNG_PRIVATE))?;
    // BYTE x 4: a 32-bit little-endian offset in the value field itself.
    let priv_offset = u32::from_le_bytes(buf.get(private.3..private.3 + 4)?.try_into().ok()?);
    let priv_ifd = t.entries(priv_offset as usize)?;
    let sony_offset = t.uint(find(&priv_ifd, 0x7200)?, 0, 0)? as usize;
    let sony_length = t.uint(find(&priv_ifd, 0x7201)?, 0, 0)? as usize;
    // UNDEFINED x 4: a 32-bit little-endian value in the value field itself.
    let key_entry = find(&priv_ifd, 0x7221)?;
    let key = u32::from_le_bytes(buf.get(key_entry.3..key_entry.3 + 4)?.try_into().ok()?);
    if sony_length == 0 || sony_length > (1 << 20) {
        return None;
    }
    let block = buf.get(sony_offset..sony_offset.checked_add(sony_length / 4 * 4)?)?;
    let decrypted = sony_decrypt(block, key);
    // The decrypted block is an IFD at 0 whose value offsets are relative to the file.
    let sr2 = Tiff { buf: &decrypted, le: true };
    let sr2_entries = sr2.entries(0)?;
    let levels = |tag: u16| -> Option<[f32; 4]> {
        let e = find(&sr2_entries, tag)?;
        let mut v = [0.0f32; 4];
        for (i, x) in v.iter_mut().enumerate() {
            *x = sr2.uint(e, i, sony_offset)? as f32;
        }
        Some(v)
    };
    // The decoder's channel mapping and its `normalize_wb`.
    let raw_wb = if let Some(l) = levels(0x7303) { [l[1], l[0], l[3], l[2]] } else { levels(0x7313)? };
    let div = raw_wb[1];
    let mut norm = raw_wb;
    norm.iter_mut().for_each(|v| {
        if v.is_normal() {
            *v /= div;
        }
    });
    Some(WbSource { wb_coeffs: [norm[0], (norm[1] + norm[2]) / 2.0, norm[3], f32::NAN], color_matrix: camera.color_matrix.clone() })
}

// `..Default::default()` keeps this building when `CaptureInfo` gains fields.
#[allow(clippy::needless_update)]
fn capture_from(md: &RawMetadata, header: impl FnOnce() -> Option<WbSource>, path: &Path) -> CaptureInfo {
    let e = &md.exif;
    let nonempty = |s: &str| {
        let s = s.trim().trim_matches('\0').trim();
        (!s.is_empty()).then(|| s.to_string())
    };
    let iso = e
        .iso_speed_ratings
        .map(u32::from)
        .filter(|v| *v != 0 && *v != 65535)
        .or(e.iso_speed.filter(|v| *v != 0))
        .or(e.recommended_exposure_index.filter(|v| *v != 0));
    let dng = read_dng_ifd0(path);
    // A DNG's own tags first (cheap, and identical for `read_capture_info` and `load`); the decoded
    // header (camera database, maker notes) for everything else.
    let white = dng
        .as_ref()
        .and_then(|d| white_from_neutral(d.neutral?, d.matrices.clone()))
        .or_else(|| header().as_ref().and_then(as_shot_white));
    CaptureInfo {
        make: nonempty(&md.make),
        model: nonempty(&md.model),
        lens: e.lens_model.as_deref().and_then(nonempty),
        iso: iso.map(|v| v as f32),
        shutter_s: positive(e.exposure_time),
        aperture: positive(e.fnumber),
        flash_fired: e.flash.map(|v| v & 1 == 1),
        raw: Some(true),
        captured_unix_ms: e.date_time_original.as_deref().and_then(crate::exif_date),
        as_shot_kelvin: white.map(|w| w.0),
        as_shot_tint: white.map(|w| w.1),
        baseline_exposure: dng.as_ref().and_then(|d| d.baseline),
        focal_mm: positive(e.focal_length),
        focal_35mm: read_focal_35mm(path),
        focal_read: true,
        ..Default::default()
    }
}

fn orientation_of(md: &RawMetadata) -> u16 {
    md.exif.orientation.filter(|o| (1..=8).contains(o)).unwrap_or(1)
}

pub fn read_capture_info(path: &Path) -> Result<CaptureInfo, IoError> {
    guard("raw", || {
        let src = open_source(path)?;
        let decoder = rawler::get_decoder(&src).map_err(rawler_err)?;
        let params = RawDecodeParams::default();
        let md = decoder.raw_metadata(&src, &params).map_err(rawler_err)?;
        // Files without usable DNG tags need the decoded-image header (maker-note white balance,
        // camera database matrices); a dummy decode reads it without touching the sensor data.
        // Sony ARW: read the encrypted SR2 block directly (a dummy decode copies the whole file).
        let header = || arw_wb_source(src.buf()).or_else(|| decoder.raw_image(&src, &params, true).ok().map(|r| WbSource::of(&r)));
        Ok(capture_from(&md, header, path))
    })
}

/// EXIF-like fields from the RAW decoder, for files whose container the EXIF reader cannot open.
/// Empty on any problem.
pub(crate) fn export_metadata_fallback(path: &Path) -> crate::ExportMetadata {
    use crate::meta::{exposure_rational, tenth_rational};
    guard("raw", || {
        let src = open_source(path)?;
        let decoder = rawler::get_decoder(&src).map_err(rawler_err)?;
        let md = decoder.raw_metadata(&src, &RawDecodeParams::default()).map_err(rawler_err)?;
        let e = &md.exif;
        let clean = |s: &str| {
            let s = s.trim().trim_matches('\0').trim();
            (!s.is_empty()).then(|| s.to_string())
        };
        let pair = |r: Option<rawler::formats::tiff::Rational>| r.filter(|r| r.d != 0 && r.n != 0).map(|r| (r.n, r.d));
        let iso = e.iso_speed_ratings.map(u32::from).filter(|v| *v != 0 && *v != 65535).or(e.iso_speed.filter(|v| *v != 0));
        Ok(crate::ExportMetadata {
            make: clean(&md.make),
            model: clean(&md.model),
            lens: e.lens_model.as_deref().and_then(clean),
            iso,
            exposure_time: positive(e.exposure_time).and_then(exposure_rational),
            f_number: positive(e.fnumber).and_then(tenth_rational),
            focal_length: pair(e.focal_length),
            datetime_original: e.date_time_original.clone().filter(|s| s.len() == 19),
            artist: e.artist.as_deref().and_then(clean),
            copyright: e.copyright.as_deref().and_then(clean),
            software: None,
            gps: e.gps.as_ref().and_then(|g| {
                let dms = |v: &Option<[rawler::formats::tiff::Rational; 3]>| -> Vec<f64> {
                    v.iter().flatten().map(|r| if r.d == 0 { f64::NAN } else { r.n as f64 / r.d as f64 }).collect()
                };
                let first = |r: &Option<String>| r.as_deref().and_then(|s| s.trim().chars().next());
                let alt = g.gps_altitude.filter(|r| r.d != 0).map(|r| r.n as f64 / r.d as f64);
                crate::meta::gps_from_parts(
                    &dms(&g.gps_latitude),
                    first(&g.gps_latitude_ref),
                    &dms(&g.gps_longitude),
                    first(&g.gps_longitude_ref),
                    alt,
                    g.gps_altitude_ref == Some(1),
                )
            }),
            description: None,
            keywords: Vec::new(),
        })
    })
    .unwrap_or_default()
}

// ---------------------------------------------------------------- colour

fn det3(m: &Mat3) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

/// xy chromaticity of the DNG/EXIF calibration illuminants we can adapt from.
fn illuminant_xy(i: &Illuminant) -> Option<(f64, f64)> {
    Some(match i {
        Illuminant::D65 => color::D65_XY,
        Illuminant::A | Illuminant::Tungsten | Illuminant::IsoStudioTungsten => (0.44757, 0.40745),
        Illuminant::B => (0.34842, 0.35161),
        Illuminant::C => (0.31006, 0.31616),
        Illuminant::D50 => (0.34567, 0.35850),
        Illuminant::D55 | Illuminant::Daylight | Illuminant::FineWeather => (0.33242, 0.34743),
        Illuminant::D75 => (0.29902, 0.31485),
        _ => return None,
    })
}

/// The file's colour matrix as `XYZ(D65) -> camera` (3 rows).
fn xyz_d65_to_camera(raw: &RawImage) -> Result<Mat3, IoError> {
    let order = [
        Illuminant::D65,
        Illuminant::A,
        Illuminant::B,
        Illuminant::C,
        Illuminant::D50,
        Illuminant::D55,
        Illuminant::D75,
        Illuminant::Daylight,
        Illuminant::FineWeather,
        Illuminant::Tungsten,
    ];
    let (illu, flat) = raw
        .color_matrix_find_first(order)
        .ok_or_else(|| IoError::Unsupported("file has no usable colour matrix".into()))?;
    if flat.len() < 9 || flat.len() % 3 != 0 || flat.iter().any(|v| !v.is_finite()) {
        return Err(IoError::Decode("invalid colour matrix".into()));
    }
    let mut m = [[0.0f64; 3]; 3];
    for (r, row) in m.iter_mut().enumerate() {
        for (c, v) in row.iter_mut().enumerate() {
            *v = flat[r * 3 + c] as f64;
        }
    }
    if illu != Illuminant::D65 {
        let Some(xy) = illuminant_xy(&illu) else {
            return Err(IoError::Unsupported(format!("colour matrix for illuminant {illu:?}")));
        };
        let a = color::adaptation(color::xy_to_xyz(color::D65_XY), color::xy_to_xyz(xy));
        m = color::mul(&m, &a);
    }
    Ok(m)
}

/// Colour transform of one image: as-shot white balance (per camera channel, green = 1) and the
/// matrix that takes white-balanced camera RGB to ACEScg.
struct ColourXform {
    wb: [f32; 3],
    m: [[f32; 3]; 3],
}

fn camera_to_acescg(raw: &RawImage) -> Result<ColourXform, IoError> {
    let mut xyz2cam = xyz_d65_to_camera(raw)?;
    // Scale rows so that D65 white maps to camera (1,1,1): then a white-balanced neutral is neutral.
    let w = color::xy_to_xyz(color::D65_XY);
    for row in xyz2cam.iter_mut() {
        let s: f64 = row.iter().zip(w).map(|(a, b)| a * b).sum();
        if !s.is_finite() || s.abs() < 1e-9 {
            return Err(IoError::Decode("degenerate colour matrix".into()));
        }
        row.iter_mut().for_each(|v| *v /= s);
    }
    if det3(&xyz2cam).abs() < 1e-9 {
        return Err(IoError::Decode("singular colour matrix".into()));
    }
    let cam2xyz = color::invert(&xyz2cam);
    let cat = color::adaptation(color::xy_to_xyz(color::D65_XY), color::xy_to_xyz(color::ACES_WHITE_XY));
    let xyz2ap1 = color::invert(&color::AP1_TO_XYZ);
    let m = color::mul(&xyz2ap1, &color::mul(&cat, &cam2xyz));

    // As-shot white balance, normalised to green.
    let c = raw.wb_coeffs;
    let wb = if c[0].is_finite() && c[1].is_finite() && c[2].is_finite() && c[0] > 0.0 && c[1] > 0.0 && c[2] > 0.0 {
        [(c[0] / c[1]) as f64, 1.0, (c[2] / c[1]) as f64]
    } else {
        [1.0, 1.0, 1.0]
    };
    Ok(ColourXform { wb: [wb[0] as f32, 1.0, wb[2] as f32], m: color::to_f32(&m) })
}

// ---------------------------------------------------------------- sensor data

/// Black / white level lookup of one raw image.
struct Levels {
    cpp: usize,
    bw: usize,
    bh: usize,
    bcpp: usize,
    black: Vec<f32>,
    white: Vec<u32>,
}

impl Levels {
    fn new(raw: &RawImage) -> Result<Levels, IoError> {
        let bl = &raw.blacklevel;
        let (bw, bh, bcpp) = (bl.width.max(1), bl.height.max(1), bl.cpp.max(1));
        let black = bl.as_vec();
        if black.is_empty() || black.len() != bw * bh * bcpp {
            return Err(IoError::Decode("inconsistent black level table".into()));
        }
        let white = raw.whitelevel.0.clone();
        if white.is_empty() {
            return Err(IoError::Decode("missing white level".into()));
        }
        let lv = Levels { cpp: raw.cpp, bw, bh, bcpp, black, white };
        // Reject a degenerate range early.
        for y in 0..bh.max(2) {
            for x in 0..bw.max(2) {
                for c in 0..raw.cpp {
                    if lv.white(x, y, c) - lv.black(x, y, c) <= 0.0 {
                        return Err(IoError::Decode("white level is not above black level".into()));
                    }
                }
            }
        }
        Ok(lv)
    }

    /// White level per channel (cpp==3), per 2x2 position (4 entries) or single.
    #[inline]
    fn white(&self, x: usize, y: usize, c: usize) -> f32 {
        if self.cpp == 1 && self.white.len() == 4 {
            self.white[(y & 1) * 2 + (x & 1)] as f32
        } else if self.white.len() == self.cpp {
            self.white[c] as f32
        } else {
            self.white[0] as f32
        }
    }

    #[inline]
    fn black(&self, x: usize, y: usize, c: usize) -> f32 {
        let cc = if self.bcpp == self.cpp { c } else { 0 };
        self.black[(y % self.bh) * self.bw * self.bcpp + (x % self.bw) * self.bcpp + cc]
    }

    /// (black, white) per 2x2 sensor position, if the tables have period 2 (always true for Bayer).
    fn bayer_tables(&self) -> Option<([f32; 4], [f32; 4])> {
        if self.cpp != 1 || self.bw > 2 || self.bh > 2 || (self.white.len() != 1 && self.white.len() != 4) {
            return None;
        }
        let mut b = [0.0; 4];
        let mut w = [0.0; 4];
        for y in 0..2 {
            for x in 0..2 {
                b[y * 2 + x] = self.black(x, y, 0);
                w[y * 2 + x] = self.white(x, y, 0);
            }
        }
        Some((b, w))
    }
}

fn check_len(raw: &RawImage) -> Result<(), IoError> {
    let len = match &raw.data {
        RawImageData::Integer(v) => v.len(),
        RawImageData::Float(v) => v.len(),
    };
    if len != raw.width * raw.height * raw.cpp {
        return Err(IoError::Decode("raw buffer size does not match image size".into()));
    }
    Ok(())
}

/// Black/white normalisation without clipping; returns `width*height*cpp` floats.
fn normalise(raw: &RawImage, lv: &Levels) -> Vec<f32> {
    let (w, h, cpp) = (raw.width, raw.height, raw.cpp);
    let mut out = vec![0.0f32; w * h * cpp];
    out.par_chunks_mut(w * cpp).enumerate().for_each(|(y, row)| {
        let base = y * w * cpp;
        for x in 0..w {
            for c in 0..cpp {
                let v = match &raw.data {
                    RawImageData::Integer(d) => d[base + x * cpp + c] as f32,
                    RawImageData::Float(d) => d[base + x * cpp + c],
                };
                let b = lv.black(x, y, c);
                row[x * cpp + c] = (v - b) / (lv.white(x, y, c) - b);
            }
        }
    });
    out
}

/// Demosaiced (or already RGB) camera data plus the rectangle of it that is the final image.
struct Developed {
    rgb: Color2D<f32, 3>,
    crop: Rect,
}

fn fits(r: &Rect, outer: &Rect) -> bool {
    r.p.x + r.d.w <= outer.p.x + outer.d.w && r.p.y + r.d.h <= outer.p.y + outer.d.h
}

/// Demosaic region (active area) and final crop of a CFA image, both in sensor coordinates.
fn cfa_regions(raw: &RawImage) -> Result<(Rect, Rect), IoError> {
    let full = Rect::new(Point::default(), Dim2::new(raw.width, raw.height));
    let roi = raw.active_area.filter(|a| fits(a, &full)).unwrap_or(full);
    if roi.d.w < 16 || roi.d.h < 16 {
        return Err(IoError::Decode("image area too small to demosaic".into()));
    }
    // The default crop must lie inside the demosaiced region, otherwise the region itself is used.
    let crop = raw
        .crop_area
        .filter(|c| fits(c, &roi) && c.p.x >= roi.p.x && c.p.y >= roi.p.y && !c.d.is_empty())
        .unwrap_or(roi);
    Ok((roi, crop))
}

fn develop_sensor(raw: &RawImage, lv: &Levels, st: &mut Stages) -> Result<Developed, IoError> {
    let full = Rect::new(Point::default(), Dim2::new(raw.width, raw.height));
    match &raw.photometric {
        RawPhotometricInterpretation::Cfa(cfg) if raw.cpp == 1 => {
            let (roi, crop_s) = cfa_regions(raw)?;
            let data = st.time("normalise", || normalise(raw, lv));
            let mosaic = PixF32::new_with(data, raw.width, raw.height);
            let rgb = st.time("demosaic", || {
                if cfg.cfa.is_rgb() && cfg.sensor == SensorType::Bayer {
                    Ok(PPGDemosaic::new().demosaic(&mosaic, &cfg.cfa, &cfg.colors, roi))
                } else if cfg.cfa.is_rgb() && cfg.sensor == SensorType::Xtrans {
                    Ok(XTransBilinearDemosaic::new().demosaic(&mosaic, &cfg.cfa, &cfg.colors, roi))
                } else {
                    Err(IoError::Unsupported(format!(
                        "colour filter array with {} colours (only RGB Bayer / X-Trans are developed)",
                        cfg.cfa.unique_colors()
                    )))
                }
            })?;
            // The demosaiced buffer starts at `roi`.
            let crop = Rect::new(Point::new(crop_s.p.x - roi.p.x, crop_s.p.y - roi.p.y), crop_s.d);
            Ok(Developed { rgb, crop })
        }
        RawPhotometricInterpretation::LinearRaw if raw.cpp == 3 => {
            let data = st.time("normalise", || normalise(raw, lv));
            let px: Vec<[f32; 3]> = data.as_chunks::<3>().0.to_vec();
            let rgb = Color2D::<f32, 3>::new_with(px, raw.width, raw.height);
            let crop = raw.crop_area.or(raw.active_area).filter(|c| fits(c, &full) && !c.d.is_empty()).unwrap_or(full);
            Ok(Developed { rgb, crop })
        }
        other => Err(IoError::Unsupported(format!(
            "sensor layout (photometric {:?}, {} samples per pixel)",
            std::mem::discriminant(other),
            raw.cpp
        ))),
    }
}

/// Sensor-clip neutralisation (see module docs): below `CLIP_SOFT_START` (normalised sensor value)
/// nothing changes; at 1.0 (sensor clip) the pixel is fully neutral.
const CLIP_SOFT_START: f32 = 0.90;

/// Applies the as-shot white balance and, if `neutralize`, neutralises pixels in which a camera
/// channel hit the sensor clip level: there the colour is unknowable (the clipped channel would
/// have been higher), so instead of a magenta/green cast the pixel gets the brightest
/// white-balanced channel in all three. The blend is smooth so that no ring appears around
/// clipped regions. Values are not limited: highlights stay above 1.0.
#[inline]
fn white_balance_pixel(p: &[f32; 3], wb: &[f32; 3], neutralize: bool) -> [f32; 3] {
    let v = [p[0] * wb[0], p[1] * wb[1], p[2] * wb[2]];
    let sensor_max = p[0].max(p[1]).max(p[2]);
    if !neutralize || sensor_max <= CLIP_SOFT_START {
        return v;
    }
    let t = ((sensor_max - CLIP_SOFT_START) / (1.0 - CLIP_SOFT_START)).min(1.0);
    let w = t * t * (3.0 - 2.0 * t);
    let mx = v[0].max(v[1]).max(v[2]);
    [v[0] + w * (mx - v[0]), v[1] + w * (mx - v[1]), v[2] + w * (mx - v[2])]
}

#[inline]
fn mat_mul(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// Full resolution: white balance + clip handling + colour matrix in one pass. Output RGBA f32.
fn convert_full(dev: &Developed, x: &ColourXform, neutralize: bool, gain: f32) -> (usize, usize, Vec<f32>) {
    let (cw, ch) = (dev.crop.d.w, dev.crop.d.h);
    let stride = dev.rgb.width;
    let px = dev.rgb.pixels();
    let mut out = vec![0.0f32; cw * ch * 4];
    out.par_chunks_mut(cw * 4).enumerate().for_each(|(y, row)| {
        let base = (dev.crop.p.y + y) * stride + dev.crop.p.x;
        for (o, p) in row.as_chunks_mut::<4>().0.iter_mut().zip(&px[base..base + cw]) {
            let c = mat_mul(&x.m, white_balance_pixel(p, &x.wb, neutralize));
            *o = [c[0] * gain, c[1] * gain, c[2] * gain, 1.0];
        }
    });
    (cw, ch, out)
}

/// White-balances and box-filters the crop of `dev.rgb` by integer factor `f` (>= 2).
fn box_wb(dev: &Developed, f: usize, x: &ColourXform, neutralize: bool) -> (usize, usize, Vec<[f32; 3]>) {
    let (cw, ch) = (dev.crop.d.w, dev.crop.d.h);
    let (ow, oh) = ((cw / f).max(1), (ch / f).max(1));
    let stride = dev.rgb.width;
    let px = dev.rgb.pixels();
    let mut out = vec![[0.0f32; 3]; ow * oh];
    out.par_chunks_mut(ow).enumerate().for_each(|(oy, row)| {
        let y0 = oy * f;
        let y1 = (y0 + f).min(ch);
        for (ox, o) in row.iter_mut().enumerate() {
            let x0 = ox * f;
            let x1 = (x0 + f).min(cw);
            let mut acc = [0.0f32; 3];
            for y in y0..y1 {
                let base = (dev.crop.p.y + y) * stride + dev.crop.p.x;
                for p in &px[base + x0..base + x1] {
                    let v = white_balance_pixel(p, &x.wb, neutralize);
                    acc[0] += v[0];
                    acc[1] += v[1];
                    acc[2] += v[2];
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as f32;
            *o = [acc[0] / n, acc[1] / n, acc[2] / n];
        }
    });
    (ow, oh, out)
}

/// Fast proxy for Bayer sensors: every output pixel is the per-colour mean of the `f` x `f`
/// block of CFA samples at that position (for `f == 2` exactly R, mean of the two G, B of one 2x2
/// cell: the "superpixel"). Needs no demosaic. Black/white normalisation is fused in. Blocks start
/// at the crop origin, exactly like the box filter of the full path, so sizes are identical.
/// Output: normalised camera RGB (not yet white-balanced), `ow * oh` pixels.
fn bin_bayer(
    raw: &RawImage,
    tables: ([f32; 4], [f32; 4]),
    colours: [usize; 4],
    crop: Rect,
    f: usize,
) -> (usize, usize, Vec<[f32; 3]>) {
    let (ow, oh) = ((crop.d.w / f).max(1), (crop.d.h / f).max(1));
    let (black, white) = tables;
    let scale = [
        1.0 / (white[0] - black[0]),
        1.0 / (white[1] - black[1]),
        1.0 / (white[2] - black[2]),
        1.0 / (white[3] - black[3]),
    ];
    let w = raw.width;
    let mut out = vec![[0.0f32; 3]; ow * oh];
    out.par_chunks_mut(ow).enumerate().for_each(|(oy, row)| {
        for (ox, o) in row.iter_mut().enumerate() {
            let mut acc = [0.0f32; 3];
            let mut cnt = [0.0f32; 3];
            // Blocks are full except when `f` exceeds the image (tiny `max_dim`): clamp like the box filter.
            let (bh, bw) = (f.min(crop.d.h - oy * f), f.min(crop.d.w - ox * f));
            for dy in 0..bh {
                let sy = crop.p.y + oy * f + dy;
                for dx in 0..bw {
                    let sx = crop.p.x + ox * f + dx;
                    let k = (sy & 1) * 2 + (sx & 1);
                    let v = match &raw.data {
                        RawImageData::Integer(d) => d[sy * w + sx] as f32,
                        RawImageData::Float(d) => d[sy * w + sx],
                    };
                    let c = colours[k];
                    acc[c] += (v - black[k]) * scale[k];
                    cnt[c] += 1.0;
                }
            }
            *o = [acc[0] / cnt[0].max(1.0), acc[1] / cnt[1].max(1.0), acc[2] / cnt[2].max(1.0)];
        }
    });
    (ow, oh, out)
}

/// Which demosaic strategy a proxy decode may use.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Default: fast binned path for Bayer sensors when the reduction factor is >= 2.
    Auto,
    /// Always demosaic at full resolution first, then box-filter (the reference path).
    FullDemosaic,
}

/// Wall-clock time of the stages of one decode, in milliseconds.
#[doc(hidden)]
#[derive(Default, Debug, Clone)]
pub struct Stages(pub Vec<(&'static str, f64)>);

impl Stages {
    fn time<T>(&mut self, name: &'static str, f: impl FnOnce() -> T) -> T {
        let t = std::time::Instant::now();
        let r = f();
        self.0.push((name, t.elapsed().as_secs_f64() * 1000.0));
        r
    }
}

pub fn load(path: &Path, options: &LoadOptions) -> Result<Loaded, IoError> {
    load_with_mode(path, options, Mode::Auto).map(|(l, _)| l)
}

/// Like [`load`] but lets the caller pick the strategy and returns per-stage timings.
#[doc(hidden)]
pub fn load_with_mode(path: &Path, options: &LoadOptions, mode: Mode) -> Result<(Loaded, Stages), IoError> {
    guard("raw", || load_inner(path, options, mode))
}

type FastPlan = (([f32; 4], [f32; 4]), [usize; 4], Rect, usize);

fn load_inner(path: &Path, options: &LoadOptions, mode: Mode) -> Result<(Loaded, Stages), IoError> {
    let mut st = Stages::default();
    let neutralize = options.neutralize_clipped_highlights;
    let src = st.time("file read", || open_source(path))?;
    let decoder = st.time("container", || rawler::get_decoder(&src).map_err(rawler_err))?;
    let params = RawDecodeParams::default();
    let md = decoder.raw_metadata(&src, &params).map_err(rawler_err)?;
    let raw = st.time("decompress", || decoder.raw_image(&src, &params, false).map_err(rawler_err))?;
    let capture = capture_from(&md, || Some(WbSource::of(&raw)), path);
    let orientation = orientation_of(&md);
    // Multiply after the matrix exactly as in the former separate baseline pass.
    let gain = 2.0f32.powf(capture.baseline_exposure.filter(|v| v.is_finite()).unwrap_or(0.35) + 1.6);

    if raw.fuji_rotation_width.is_some() {
        return Err(IoError::Unsupported("Fuji rotated (SuperCCD) sensors".into()));
    }
    check_len(&raw)?;
    let xform = camera_to_acescg(&raw)?;
    let lv = Levels::new(&raw)?;
    let factor_for = |cw: usize, ch: usize| match options.max_dim {
        Some(n) => (cw.max(ch) as u32).div_ceil(n.max(1)).max(1) as usize,
        None => 1,
    };

    // Fast path: Bayer + reduction >= 2 -> bin CFA samples directly, no demosaic.
    let mut fast: Option<FastPlan> = None;
    if let (Mode::Auto, RawPhotometricInterpretation::Cfa(cfg), 1) = (mode, &raw.photometric, raw.cpp) {
        if cfg.cfa.is_rgb() && cfg.sensor == SensorType::Bayer {
            if let (Some(tables), Ok((_, crop))) = (lv.bayer_tables(), cfa_regions(&raw)) {
                let f = factor_for(crop.d.w, crop.d.h);
                let colours = [cfg.cfa.color_at(0, 0), cfg.cfa.color_at(0, 1), cfg.cfa.color_at(1, 0), cfg.cfa.color_at(1, 1)];
                if f >= 2 && colours.iter().all(|c| *c < 3) {
                    fast = Some((tables, colours, crop, f));
                }
            }
        }
    }

    let (cw, ch, ow, oh, small);
    if let Some((tables, colours, crop, f)) = fast {
        let (a, b, v) = st.time("bin (no demosaic)", || bin_bayer(&raw, tables, colours, crop, f));
        drop(raw);
        let v = st.time("white balance + clip", || {
            let mut v = v;
            v.par_iter_mut().for_each(|p| *p = white_balance_pixel(p, &xform.wb, neutralize));
            v
        });
        (cw, ch, ow, oh, small) = (crop.d.w, crop.d.h, a, b, v);
    } else {
        let dev = develop_sensor(&raw, &lv, &mut st)?;
        drop(raw);
        let (cw0, ch0) = (dev.crop.d.w, dev.crop.d.h);
        let f = factor_for(cw0, ch0);
        if f == 1 {
            let (w, h, data) = st.time("white balance + clip + matrix + baseline", || convert_full(&dev, &xform, neutralize, gain));
            drop(dev);
            return finish(st, w, h, data, orientation, capture, (cw0, ch0));
        }
        let (a, b, v) = st.time("white balance + clip + downscale", || box_wb(&dev, f, &xform, neutralize));
        (cw, ch, ow, oh, small) = (cw0, ch0, a, b, v);
    }

    let data = st.time("colour matrix + baseline", || {
        let mut out = vec![0.0f32; ow * oh * 4];
        out.par_chunks_mut(4).zip(small.par_iter()).for_each(|(o, p)| {
            let c = mat_mul(&xform.m, *p);
            o.copy_from_slice(&[c[0] * gain, c[1] * gain, c[2] * gain, 1.0]);
        });
        out
    });
    finish(st, ow, oh, data, orientation, capture, (cw, ch))
}

fn finish(
    mut st: Stages,
    w: usize,
    h: usize,
    data: Vec<f32>,
    orientation: u16,
    capture: CaptureInfo,
    crop_size: (usize, usize),
) -> Result<(Loaded, Stages), IoError> {
    let (w, h, data) = st.time("orientation", || orient_rgba(w as u32, h as u32, data, orientation));
    let image = ImageBuf::from_rgba(w, h, data).ok_or_else(|| IoError::Decode("internal buffer size mismatch".into()))?;
    let full_size = oriented_size(crop_size.0 as u32, crop_size.1 as u32, orientation);
    Ok((Loaded { image, capture, kind: SourceKind::Raw, full_size, profile: crate::InputProfile::Untagged }, st))
}

/// Embedded previews are stored in sensor orientation for some cameras and already upright for
/// others. A sensor is landscape, so a still-landscape preview of a portrait-oriented shot has
/// not been rotated yet; one that is already portrait has.
fn upright_embedded(img: image::DynamicImage, orientation: u16) -> image::DynamicImage {
    let mut img = img;
    if swaps_axes(orientation) && img.width() > img.height() {
        if let Some(o) = image::metadata::Orientation::from_exif(orientation as u8) {
            img.apply_orientation(o);
        }
    }
    img
}

/// Extracts a camera preview without falling back to sensor decode.
pub fn load_embedded_preview(path: &Path, max_dim: u32) -> Result<Option<PreviewPixels>, IoError> {
    embedded_preview(path, max_dim.max(1), 1)
}

type PreviewPixels = (u32, u32, Vec<u8>);

fn embedded_preview(path: &Path, max_dim: u32, min_dim: u32) -> Result<Option<PreviewPixels>, IoError> {
    guard("raw", || {
        let src = open_source(path)?;
        let decoder = rawler::get_decoder(&src).map_err(rawler_err)?;
        let params = RawDecodeParams::default();
        let md = decoder.raw_metadata(&src, &params).map_err(rawler_err)?;
        let orientation = orientation_of(&md);
        let icc = |img: &image::DynamicImage| crate::icc::find_preview_icc(src.buf(), img.width(), img.height());
        // Accept an embedded image only if it is not much smaller than requested.
        let good = |i: &image::DynamicImage| i.width().max(i.height()) >= min_dim;
        if let Ok(Some(img)) = decoder.preview_image(&src, &params) {
            if good(&img) {
                let profile = icc(&img);
                return Ok(Some((upright_embedded(img, orientation), profile)));
            }
        }
        if let Ok(Some(img)) = decoder.thumbnail_image(&src, &params) {
            if good(&img) {
                let profile = icc(&img);
                return Ok(Some((upright_embedded(img, orientation), profile)));
            }
        }
        Ok(None)
    }).map(|found| found.map(|(img, icc)| {
        // The preview is sRGB pixels unless its JPEG carries a profile (e.g. Adobe RGB).
        let (w, h, mut rgba) = crate::dynamic_to_thumb(img, max_dim);
        crate::icc::thumbnail_to_srgb(icc.as_deref(), &mut rgba);
        (w, h, rgba)
    }))
}

pub fn load_thumbnail(path: &Path, max_dim: u32) -> Result<PreviewPixels, IoError> {
    if let Ok(Some(img)) = embedded_preview(path, max_dim, max_dim / 2) {
        return Ok(img);
    }
    // No (usable) embedded preview: decode a proxy and encode it as sRGB.
    let loaded = load(path, &LoadOptions { max_dim: Some(max_dim), ..Default::default() })?;
    let (w, h) = (loaded.image.width(), loaded.image.height());
    let m = color::to_f32(&color::acescg_to_rec709());
    let mut out = vec![255u8; w as usize * h as usize * 4];
    out.par_chunks_exact_mut(4).zip(loaded.image.data().par_chunks_exact(4)).for_each(|(o, s)| {
        for k in 0..3 {
            let lin = m[k][0] * s[0] + m[k][1] * s[1] + m[k][2] * s[2];
            o[k] = (color::srgb_oetf(lin).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
    });
    Ok((w, h, out))
}

#[cfg(test)]
mod focal_tests {
    use super::*;

    /// A TIFF with IFD0 -> Exif IFD holding one entry `tag` (SHORT, `value`), little or big endian.
    fn tiff(le: bool, exif_tag: u16, value: u16) -> Vec<u8> {
        let u16b = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let u32b = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let mut t: Vec<u8> = if le { vec![b'I', b'I', 42, 0] } else { vec![b'M', b'M', 0, 42] };
        t.extend(u32b(8));
        // IFD0 at 8: one entry (Exif IFD pointer to 26), next IFD 0.
        t.extend(u16b(1));
        t.extend(u16b(0x8769));
        t.extend(u16b(4));
        t.extend(u32b(1));
        t.extend(u32b(26));
        t.extend(u32b(0));
        // Exif IFD at 26: one SHORT entry (the value sits in the first two bytes of the field).
        t.extend(u16b(1));
        t.extend(u16b(exif_tag));
        t.extend(u16b(3));
        t.extend(u32b(1));
        t.extend(u16b(value));
        t.extend(u16b(0));
        t.extend(u32b(0));
        t
    }

    fn read(name: &str, bytes: &[u8]) -> Option<f32> {
        let path = std::env::temp_dir().join(format!("rawgraph-focal-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let out = read_focal_35mm(&path);
        let _ = std::fs::remove_file(&path);
        out
    }

    #[test]
    fn the_35mm_focal_length_is_read_from_the_exif_ifd_of_either_byte_order() {
        assert_eq!(read("le.tif", &tiff(true, 0xA405, 35)), Some(35.0));
        assert_eq!(read("be.tif", &tiff(false, 0xA405, 24)), Some(24.0));
    }

    #[test]
    fn missing_zero_and_implausible_values_and_foreign_files_give_none() {
        assert_eq!(read("other.tif", &tiff(true, 0xA406, 35)), None, "another tag");
        assert_eq!(read("zero.tif", &tiff(true, 0xA405, 0)), None, "0 means unknown");
        assert_eq!(read("huge.tif", &tiff(true, 0xA405, 5000)), None);
        assert_eq!(read("png.png", b"\x89PNG\r\n\x1a\n0000000000000000"), None, "not a TIFF container");
        assert_eq!(read("empty.tif", &[]), None);
        let mut cut = tiff(true, 0xA405, 35);
        cut.truncate(30);
        assert_eq!(read("cut.tif", &cut), None, "truncated Exif IFD");
        let mut loop_ptr = tiff(true, 0xA405, 35);
        loop_ptr[18..22].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert_eq!(read("far.tif", &loop_ptr), None, "pointer beyond the file");
    }
}
