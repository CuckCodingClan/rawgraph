//! Lens correction data stored in DNG files: `OpcodeList3` (tag 51022), DNG specification 1.4,
//! chapter 7.
//!
//! The opcode list is always big endian, whatever the byte order of the TIFF container: `u32`
//! count, then per opcode `u32 id, u32 minimum DNG version, u32 flags, u32 parameter bytes` and
//! the parameters. Two opcodes are understood, every other one is skipped by its byte count:
//!
//! * id 1 `WarpRectilinear`: `u32 N` planes (1 or 3 are used), N x 6 `f64`
//!   (kr0 kr1 kr2 kr3 kt0 kt1), then the optical centre `cx, cy` (`f64`);
//! * id 3 `FixVignetteRadial`: five `f64` (k0..k4), then the optical centre (`f64` x 2).
//!
//! **Frame of the result.** The opcodes describe the demosaiced image of the DNG *active area*
//! (`ActiveArea`, default: the whole raw image), with the optical centre normalised to that area
//! and the radius normalised so that 1 is the farthest corner from the centre. This crate's
//! decoder returns the *default crop* (`DefaultCropOrigin` / `DefaultCropSize`, relative to the
//! active area) rotated by the EXIF orientation. [`read_lens_profile`] converts the data to that
//! frame:
//!
//! * the centre is re-expressed relative to the crop and mapped through the orientation;
//! * the radius normalisation changes with the crop (the farthest corner moves): with
//!   `s = R_crop / R_area` (both measured from the optical centre, in pixels) the coefficients
//!   become `kr_i * s^(2i)`, `kt * s` and, for the vignette, `k_i * s^(2i+2)`; this is exact, not
//!   an approximation (for a crop of a few pixels `s` is 0.99..1);
//! * the orientation also permutes / flips the axes, which transforms the tangential terms: a
//!   transposition swaps kt0 and kt1, a flip of the x axis negates kt1, a flip of the y axis
//!   negates kt0 (radial terms are unchanged).
//!
//! Assumption (not verifiable without Adobe's reference renderer): the opcode area is the active
//! area, not the default crop. If a file normalises to the crop instead, the centre is off by at
//! most (crop offset / image size), a few thousandths for the cameras in use.
//!
//! Nothing here panics on file content; every failure gives `None`.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use rawgraph_core::{LensProfile, VignetteRadial, WarpRectilinear};

const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_PHOTOMETRIC: u16 = 262;
const TAG_ORIENTATION: u16 = 274;
const TAG_SUB_IFDS: u16 = 330;
const TAG_DNG_VERSION: u16 = 50706;
const TAG_DEFAULT_CROP_ORIGIN: u16 = 50719;
const TAG_DEFAULT_CROP_SIZE: u16 = 50720;
const TAG_ACTIVE_AREA: u16 = 50829;
const TAG_OPCODE_LIST_3: u16 = 51022;

const OPCODE_WARP_RECTILINEAR: u32 = 1;
const OPCODE_FIX_VIGNETTE_RADIAL: u32 = 3;

const MAX_IFDS: usize = 32;
const MAX_ENTRIES: usize = 4096;
const MAX_OPCODE_LIST_BYTES: u64 = 16 << 20;
const MAX_OPCODES: u32 = 4096;

// ---------------------------------------------------------------- opcode list

/// Parameters as stored: optical centre normalised to the opcode area.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawLens {
    pub warp: Option<WarpRectilinear>,
    pub vignette: Option<VignetteRadial>,
}

fn be32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(off..off.checked_add(4)?)?.try_into().ok()?))
}

fn be_f64(b: &[u8], off: usize) -> Option<f64> {
    Some(f64::from_be_bytes(b.get(off..off.checked_add(8)?)?.try_into().ok()?))
}

fn f64s<const N: usize>(b: &[u8], off: usize) -> Option<[f64; N]> {
    let mut out = [0.0; N];
    for (i, v) in out.iter_mut().enumerate() {
        *v = be_f64(b, off + i * 8)?;
    }
    Some(out)
}

/// Parameters of `WarpRectilinear`. `Err(())`: malformed; `Ok(None)`: well-formed but not usable
/// (a plane count other than 1 or 3, or non-finite numbers).
fn parse_warp(p: &[u8]) -> Result<Option<WarpRectilinear>, ()> {
    let n = be32(p, 0).ok_or(())? as usize;
    if !(1..=4).contains(&n) || p.len() != 4 + n * 48 + 16 {
        return Err(());
    }
    let mut planes = Vec::with_capacity(n);
    for i in 0..n {
        planes.push(f64s::<6>(p, 4 + i * 48).ok_or(())?);
    }
    let center = f64s::<2>(p, 4 + n * 48).ok_or(())?;
    let finite = planes.iter().flatten().chain(center.iter()).all(|v| v.is_finite());
    Ok((finite && (n == 1 || n == 3)).then_some(WarpRectilinear { planes, center }))
}

fn parse_vignette(p: &[u8]) -> Result<Option<VignetteRadial>, ()> {
    if p.len() != 56 {
        return Err(());
    }
    let k = f64s::<5>(p, 0).ok_or(())?;
    let center = f64s::<2>(p, 40).ok_or(())?;
    let finite = k.iter().chain(center.iter()).all(|v| v.is_finite());
    Ok(finite.then_some(VignetteRadial { k, center }))
}

/// Reads an `OpcodeList3`. `None`: truncated or malformed list. The first usable opcode of each
/// kind wins; all other opcodes are skipped.
pub(crate) fn parse_opcode_list(bytes: &[u8]) -> Option<RawLens> {
    let count = be32(bytes, 0)?;
    if count > MAX_OPCODES {
        return None;
    }
    let mut out = RawLens { warp: None, vignette: None };
    let mut pos = 4usize;
    for _ in 0..count {
        let id = be32(bytes, pos)?;
        let size = be32(bytes, pos + 12)? as usize;
        pos = pos.checked_add(16)?;
        let params = bytes.get(pos..pos.checked_add(size)?)?;
        pos += size;
        match id {
            OPCODE_WARP_RECTILINEAR => {
                let warp = parse_warp(params).ok()?;
                if out.warp.is_none() {
                    out.warp = warp;
                }
            }
            OPCODE_FIX_VIGNETTE_RADIAL => {
                let vignette = parse_vignette(params).ok()?;
                if out.vignette.is_none() {
                    out.vignette = vignette;
                }
            }
            _ => {}
        }
    }
    Some(out)
}

// ---------------------------------------------------------------- geometry

/// Where the opcode area lies relative to the image this crate returns.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Geometry {
    /// Width and height of the opcode area (the active area) in pixels.
    pub area: (f64, f64),
    /// Default crop `(x, y, w, h)` in pixels, relative to the area's top-left corner.
    pub crop: (f64, f64, f64, f64),
    /// EXIF orientation 1..=8 applied to the cropped image.
    pub orientation: u16,
}

impl Geometry {
    /// No crop, no rotation: the opcode area is the returned image.
    #[cfg(test)]
    pub(crate) fn plain(w: f64, h: f64) -> Geometry {
        Geometry { area: (w, h), crop: (0.0, 0.0, w, h), orientation: 1 }
    }

    fn valid(&self) -> bool {
        let (aw, ah) = self.area;
        let (x, y, w, h) = self.crop;
        [aw, ah, x, y, w, h].iter().all(|v| v.is_finite()) && aw > 0.0 && ah > 0.0 && w > 0.0 && h > 0.0
    }
}

/// Position of a normalised point after the EXIF orientation, and the tangential-coefficient
/// transform of the same orientation: `(swap axes, flip x, flip y)` applied in this order to the
/// cropped image (see the module docs).
fn orientation_ops(orientation: u16) -> (bool, bool, bool) {
    match orientation {
        2 => (false, true, false),
        3 => (false, true, true),
        4 => (false, false, true),
        5 => (true, false, false),
        6 => (true, true, false),
        7 => (true, true, true),
        8 => (true, false, true),
        _ => (false, false, false),
    }
}

fn orient_point(p: [f64; 2], orientation: u16) -> [f64; 2] {
    let (swap, fx, fy) = orientation_ops(orientation);
    let [mut x, mut y] = p;
    if swap {
        std::mem::swap(&mut x, &mut y);
    }
    if fx {
        x = 1.0 - x;
    }
    if fy {
        y = 1.0 - y;
    }
    [x, y]
}

/// `(kt0, kt1)` of the warp after the orientation.
fn orient_tangential(kt: [f64; 2], orientation: u16) -> [f64; 2] {
    let (swap, fx, fy) = orientation_ops(orientation);
    let [mut kt0, mut kt1] = kt;
    if swap {
        std::mem::swap(&mut kt0, &mut kt1);
    }
    if fx {
        kt1 = -kt1;
    }
    if fy {
        kt0 = -kt0;
    }
    [kt0, kt1]
}

/// Largest distance from `(cx, cy)` to a corner of the rectangle `[x0, x1] x [y0, y1]`.
fn far_corner(c: [f64; 2], x0: f64, y0: f64, x1: f64, y1: f64) -> f64 {
    let dx = (c[0] - x0).abs().max((c[0] - x1).abs());
    let dy = (c[1] - y0).abs().max((c[1] - y1).abs());
    dx.hypot(dy)
}

/// Converts opcode parameters to the upright cropped frame (see the module docs).
pub(crate) fn to_upright(raw: &RawLens, g: &Geometry) -> LensProfile {
    if !g.valid() {
        return LensProfile { warp: None, vignette: None };
    }
    let (aw, ah) = g.area;
    let (ox, oy, cw, ch) = g.crop;
    let frame = |center: [f64; 2]| -> ([f64; 2], f64) {
        let c = [center[0] * aw, center[1] * ah];
        let r_area = far_corner(c, 0.0, 0.0, aw, ah);
        let r_crop = far_corner(c, ox, oy, ox + cw, oy + ch);
        let s = if r_area > 0.0 && r_crop.is_finite() && r_crop > 0.0 { r_crop / r_area } else { 1.0 };
        let rel = [(c[0] - ox) / cw, (c[1] - oy) / ch];
        (orient_point(rel, g.orientation), s)
    };
    let warp = raw.warp.as_ref().map(|w| {
        let (center, s) = frame(w.center);
        let planes = w
            .planes
            .iter()
            .map(|p| {
                let kt = orient_tangential([p[4] * s, p[5] * s], g.orientation);
                [p[0], p[1] * s.powi(2), p[2] * s.powi(4), p[3] * s.powi(6), kt[0], kt[1]]
            })
            .collect();
        WarpRectilinear { planes, center }
    });
    let vignette = raw.vignette.as_ref().map(|v| {
        let (center, s) = frame(v.center);
        let k = std::array::from_fn(|i| v.k[i] * s.powi(2 * i as i32 + 2));
        VignetteRadial { k, center }
    });
    LensProfile { warp, vignette }
}

// ---------------------------------------------------------------- TIFF walking

#[derive(Clone, Copy)]
struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    /// The 4-byte value / offset field as stored.
    field: [u8; 4],
}

struct Ifd {
    entries: Vec<Entry>,
    next: u32,
}

impl Ifd {
    fn get(&self, tag: u16) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag)
    }
}

struct Tiff<R> {
    r: R,
    le: bool,
    len: u64,
}

fn type_size(typ: u16) -> Option<u64> {
    match typ {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

impl<R: Read + Seek> Tiff<R> {
    fn open(mut r: R) -> Option<Tiff<R>> {
        let len = r.seek(SeekFrom::End(0)).ok()?;
        r.seek(SeekFrom::Start(0)).ok()?;
        let mut head = [0u8; 8];
        r.read_exact(&mut head).ok()?;
        let le = match &head[..4] {
            b"II*\0" => true,
            b"MM\0*" => false,
            _ => return None,
        };
        Some(Tiff { r, le, len })
    }

    fn u16(&self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        if self.le {
            u16::from_le_bytes(a)
        } else {
            u16::from_be_bytes(a)
        }
    }

    fn u32(&self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        if self.le {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    }

    fn read_at(&mut self, off: u64, n: u64) -> Option<Vec<u8>> {
        if n > MAX_OPCODE_LIST_BYTES || off.checked_add(n)? > self.len {
            return None;
        }
        self.r.seek(SeekFrom::Start(off)).ok()?;
        let mut buf = vec![0u8; n as usize];
        self.r.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    fn first_ifd_offset(&mut self) -> Option<u32> {
        let h = self.read_at(4, 4)?;
        Some(self.u32(&h))
    }

    fn ifd(&mut self, off: u32) -> Option<Ifd> {
        let head = self.read_at(u64::from(off), 2)?;
        let n = self.u16(&head) as usize;
        if n == 0 || n > MAX_ENTRIES {
            return None;
        }
        let table = self.read_at(u64::from(off) + 2, n as u64 * 12 + 4)?;
        let entries = table[..n * 12]
            .as_chunks::<12>().0.iter()
            .map(|e| Entry { tag: self.u16(&e[0..2]), typ: self.u16(&e[2..4]), count: self.u32(&e[4..8]), field: [e[8], e[9], e[10], e[11]] })
            .collect();
        Some(Ifd { entries, next: self.u32(&table[n * 12..]) })
    }

    /// The raw bytes of an entry's value (inline or out of line).
    fn data(&mut self, e: &Entry) -> Option<Vec<u8>> {
        let size = type_size(e.typ)?.checked_mul(u64::from(e.count))?;
        if size <= 4 {
            return Some(e.field[..size as usize].to_vec());
        }
        let off = u64::from(self.u32(&e.field));
        self.read_at(off, size)
    }

    /// Numeric value(s) of a SHORT / LONG / RATIONAL entry.
    fn numbers(&mut self, e: &Entry) -> Option<Vec<f64>> {
        let d = self.data(e)?;
        let out = match e.typ {
            3 => d.as_chunks::<2>().0.iter().map(|c| f64::from(self.u16(c))).collect(),
            4 => d.as_chunks::<4>().0.iter().map(|c| f64::from(self.u32(c))).collect(),
            5 => d
                .as_chunks::<8>().0.iter()
                .map(|c| {
                    let (n, den) = (f64::from(self.u32(&c[..4])), f64::from(self.u32(&c[4..])));
                    if den == 0.0 {
                        f64::NAN
                    } else {
                        n / den
                    }
                })
                .collect(),
            _ => return None,
        };
        Some(out)
    }

    fn number(&mut self, ifd: &Ifd, tag: u16) -> Option<f64> {
        let e = *ifd.get(tag)?;
        self.numbers(&e)?.first().copied().filter(|v| v.is_finite())
    }
}

/// Geometry of the opcode area relative to the returned image, from the IFD holding the raw data.
fn geometry<R: Read + Seek>(t: &mut Tiff<R>, raw: &Ifd, orientation: u16) -> Option<Geometry> {
    let (w, h) = (t.number(raw, TAG_IMAGE_WIDTH)?, t.number(raw, TAG_IMAGE_LENGTH)?);
    if w < 1.0 || h < 1.0 {
        return None;
    }
    // ActiveArea: top, left, bottom, right; used only when it lies inside the image.
    let active = raw
        .get(TAG_ACTIVE_AREA)
        .copied()
        .and_then(|e| t.numbers(&e))
        .filter(|a| a.len() == 4 && a[0] >= 0.0 && a[1] >= 0.0 && a[2] > a[0] && a[3] > a[1] && a[2] <= h && a[3] <= w);
    let area = active.as_ref().map_or((w, h), |a| (a[3] - a[1], a[2] - a[0]));
    // Default crop relative to the active area; the whole area if absent or not inside it.
    let pair = |t: &mut Tiff<R>, tag| raw.get(tag).copied().and_then(|e| t.numbers(&e)).filter(|v| v.len() == 2 && v.iter().all(|x| x.is_finite()));
    let crop = match (pair(t, TAG_DEFAULT_CROP_ORIGIN), pair(t, TAG_DEFAULT_CROP_SIZE)) {
        (Some(o), Some(s)) if o[0] >= 0.0 && o[1] >= 0.0 && s[0] > 0.0 && s[1] > 0.0 && o[0] + s[0] <= area.0 && o[1] + s[1] <= area.1 => {
            (o[0], o[1], s[0], s[1])
        }
        _ => (0.0, 0.0, area.0, area.1),
    };
    Some(Geometry { area, crop, orientation })
}

fn read_profile<R: Read + Seek>(reader: R) -> Option<LensProfile> {
    let mut t = Tiff::open(reader)?;
    let first = t.first_ifd_offset()?;

    // All IFDs reachable from IFD0 (next-IFD chain and SubIFDs), at most MAX_IFDS.
    let mut ifds: Vec<Ifd> = Vec::new();
    let mut seen: Vec<u32> = Vec::new();
    let mut queue = vec![first];
    while let Some(off) = queue.pop() {
        if off == 0 || seen.contains(&off) || seen.len() >= MAX_IFDS {
            continue;
        }
        seen.push(off);
        let Some(ifd) = t.ifd(off) else { continue };
        if ifd.next != 0 {
            queue.insert(0, ifd.next);
        }
        if let Some(e) = ifd.get(TAG_SUB_IFDS).copied() {
            if let Some(d) = t.data(&e) {
                for c in d.as_chunks::<4>().0.iter().take(MAX_IFDS) {
                    queue.insert(0, t.u32(c));
                }
            }
        }
        ifds.push(ifd);
    }

    let ifd0 = ifds.first()?;
    ifd0.get(TAG_DNG_VERSION)?; // not a DNG: no lens data
    let orientation = ifd0
        .get(TAG_ORIENTATION)
        .copied()
        .and_then(|e| t.numbers(&e))
        .and_then(|v| v.first().copied())
        .map_or(1, |o| if (1.0..=8.0).contains(&o) { o as u16 } else { 1 });

    // The IFD with the opcode list; if several have one, the one of the raw image
    // (PhotometricInterpretation CFA 32803 / LinearRaw 34892).
    let with_list: Vec<&Ifd> = ifds.iter().filter(|i| i.get(TAG_OPCODE_LIST_3).is_some()).collect();
    let is_raw = |t: &mut Tiff<R>, i: &Ifd| matches!(t.number(i, TAG_PHOTOMETRIC), Some(p) if p == 32803.0 || p == 34892.0);
    let mut raw_ifd = None;
    for i in &with_list {
        if is_raw(&mut t, i) {
            raw_ifd = Some(*i);
            break;
        }
    }
    let raw_ifd = raw_ifd.or_else(|| with_list.first().copied())?;

    let entry = *raw_ifd.get(TAG_OPCODE_LIST_3)?;
    let list = t.data(&entry)?;
    let parsed = parse_opcode_list(&list)?;
    if parsed.warp.is_none() && parsed.vignette.is_none() {
        return None;
    }
    let g = geometry(&mut t, raw_ifd, orientation).unwrap_or(Geometry { area: (1.0, 1.0), crop: (0.0, 0.0, 1.0, 1.0), orientation });
    let lens = to_upright(&parsed, &g);
    (lens.warp.is_some() || lens.vignette.is_some()).then_some(lens)
}

/// The lens profile stored in a DNG file (`OpcodeList3`: `WarpRectilinear` and `FixVignetteRadial`),
/// converted to the frame of the image this crate decodes: the default crop, upright (see the
/// module docs for the exact conversion). `None` for non-DNG files, files without the tag or
/// without a usable opcode, truncated or malformed lists, and unreadable files. Reads only the
/// TIFF headers and the opcode list, never the pixel data.
pub fn read_lens_profile(path: &Path) -> Option<LensProfile> {
    let file = File::open(path).ok()?;
    catch_unwind(AssertUnwindSafe(|| read_profile(BufReader::new(file)))).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // ---- builders ------------------------------------------------------------

    fn be(v: u32) -> [u8; 4] {
        v.to_be_bytes()
    }

    fn op(id: u32, params: &[u8]) -> Vec<u8> {
        let mut o = [be(id), be(0x0104_0000), be(1), be(params.len() as u32)].concat();
        o.extend_from_slice(params);
        o
    }

    fn warp_params(planes: &[[f64; 6]], center: [f64; 2]) -> Vec<u8> {
        let mut p = be(planes.len() as u32).to_vec();
        for pl in planes {
            for v in pl {
                p.extend_from_slice(&v.to_be_bytes());
            }
        }
        for v in center {
            p.extend_from_slice(&v.to_be_bytes());
        }
        p
    }

    fn vignette_params(k: [f64; 5], center: [f64; 2]) -> Vec<u8> {
        k.iter().chain(center.iter()).flat_map(|v| v.to_be_bytes()).collect()
    }

    fn list(ops: &[Vec<u8>]) -> Vec<u8> {
        let mut l = be(ops.len() as u32).to_vec();
        for o in ops {
            l.extend_from_slice(o);
        }
        l
    }

    const PLANE: [f64; 6] = [1.0, 0.02, -0.01, 0.003, 0.0, 0.0];

    /// A minimal DNG-like TIFF. `sub`: the raw IFD is a SubIFD of IFD0 (otherwise IFD0 is the raw
    /// IFD). Little endian unless `be_container`.
    struct Spec {
        be_container: bool,
        sub: bool,
        orientation: u16,
        size: (u32, u32),
        active: Option<[u32; 4]>,
        crop: Option<([u32; 2], [u32; 2])>,
        opcodes: Option<Vec<u8>>,
        dng_version: bool,
    }

    /// Value of a test IFD entry: stored in the entry or out of line.
    enum V {
        Inline([u8; 4]),
        Out(Vec<u8>),
    }

    /// (tag, type, count, value)
    type TestEntry = (u16, u16, u32, V);

    impl Spec {
        fn new(opcodes: Vec<u8>) -> Spec {
            Spec { be_container: false, sub: false, orientation: 1, size: (6000, 4000), active: None, crop: None, opcodes: Some(opcodes), dng_version: true }
        }

        fn build(&self) -> Vec<u8> {
            let be_c = self.be_container;
            let w16 = |v: u16| if be_c { v.to_be_bytes() } else { v.to_le_bytes() };
            let w32 = |v: u32| if be_c { v.to_be_bytes() } else { v.to_le_bytes() };
            let short = |v: u16| {
                let mut d = [0u8; 4];
                d[..2].copy_from_slice(&w16(v));
                V::Inline(d)
            };
            let long = |v: u32| V::Inline(w32(v));
            let mut raw_entries: Vec<TestEntry> = vec![
                (TAG_IMAGE_WIDTH, 4, 1, long(self.size.0)),
                (TAG_IMAGE_LENGTH, 4, 1, long(self.size.1)),
                (TAG_PHOTOMETRIC, 3, 1, short(32803)),
            ];
            if let Some(a) = self.active {
                raw_entries.push((TAG_ACTIVE_AREA, 4, 4, V::Out(a.iter().flat_map(|v| w32(*v)).collect())));
            }
            if let Some((o, s)) = self.crop {
                raw_entries.push((TAG_DEFAULT_CROP_ORIGIN, 4, 2, V::Out(o.iter().flat_map(|v| w32(*v)).collect())));
                raw_entries.push((TAG_DEFAULT_CROP_SIZE, 4, 2, V::Out(s.iter().flat_map(|v| w32(*v)).collect())));
            }
            if let Some(o) = &self.opcodes {
                raw_entries.push((TAG_OPCODE_LIST_3, 7, o.len() as u32, V::Out(o.clone())));
            }
            let mut head_entries: Vec<TestEntry> = vec![(TAG_ORIENTATION, 3, 1, short(self.orientation))];
            if self.dng_version {
                head_entries.push((TAG_DNG_VERSION, 1, 4, V::Inline([1, 4, 0, 0])));
            }

            // Layout: header(8), IFD0, [raw IFD], out-of-line data.
            let (ifd0_entries, raw_ifd): (Vec<TestEntry>, Option<Vec<TestEntry>>) = if self.sub {
                head_entries.push((TAG_SUB_IFDS, 4, 1, V::Inline([0; 4]))); // patched below
                (head_entries, Some(raw_entries))
            } else {
                head_entries.extend(raw_entries);
                (head_entries, None)
            };
            let ifd_len = |n: usize| 2 + 12 * n + 4;
            let ifd0_off = 8u32;
            let raw_off = ifd0_off + ifd_len(ifd0_entries.len()) as u32;
            let mut data_off = raw_off + raw_ifd.as_ref().map_or(0, |r| ifd_len(r.len())) as u32;
            let mut out = if be_c { b"MM\0*".to_vec() } else { b"II*\0".to_vec() };
            out.extend_from_slice(&w32(ifd0_off));
            let mut blobs: Vec<u8> = Vec::new();
            let mut emit = |entries: &[(u16, u16, u32, V)], is_head_with_sub: bool, out: &mut Vec<u8>| {
                let mut sorted: Vec<&TestEntry> = entries.iter().collect();
                sorted.sort_by_key(|e| e.0);
                out.extend_from_slice(&w16(entries.len() as u16));
                for (tag, typ, count, v) in sorted {
                    out.extend_from_slice(&w16(*tag));
                    out.extend_from_slice(&w16(*typ));
                    out.extend_from_slice(&w32(*count));
                    match v {
                        V::Inline(d) => {
                            if is_head_with_sub && *tag == TAG_SUB_IFDS {
                                out.extend_from_slice(&w32(raw_off));
                            } else {
                                out.extend_from_slice(d);
                            }
                        }
                        V::Out(d) => {
                            out.extend_from_slice(&w32(data_off));
                            blobs.extend_from_slice(d);
                            data_off += d.len() as u32;
                        }
                    }
                }
                out.extend_from_slice(&w32(0));
            };
            emit(&ifd0_entries, self.sub, &mut out);
            if let Some(r) = &raw_ifd {
                emit(r, false, &mut out);
            }
            out.extend_from_slice(&blobs);
            out
        }
    }

    fn profile(spec: &Spec) -> Option<LensProfile> {
        read_profile(Cursor::new(spec.build()))
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    // ---- opcode list ---------------------------------------------------------

    #[test]
    fn one_plane_warp_and_vignette() {
        let l = list(&[op(1, &warp_params(&[PLANE], [0.52, 0.48])), op(3, &vignette_params([-0.1, 0.2, -0.3, 0.4, -0.5], [0.51, 0.49]))]);
        let raw = parse_opcode_list(&l).expect("parsed");
        let w = raw.warp.expect("warp");
        assert_eq!((w.planes.len(), w.planes[0], w.center), (1, PLANE, [0.52, 0.48]));
        let v = raw.vignette.expect("vignette");
        assert_eq!((v.k, v.center), ([-0.1, 0.2, -0.3, 0.4, -0.5], [0.51, 0.49]));
    }

    #[test]
    fn three_planes_and_order_of_opcodes_does_not_matter() {
        let planes = [PLANE, [1.0, 0.021, 0.0, 0.0, 0.001, -0.002], [1.0, 0.019, 0.0, 0.0, 0.0, 0.0]];
        let l = list(&[op(3, &vignette_params([0.1; 5], [0.5, 0.5])), op(1, &warp_params(&planes, [0.5, 0.5]))]);
        let raw = parse_opcode_list(&l).expect("parsed");
        assert_eq!(raw.warp.expect("warp").planes, planes.to_vec());
        assert!(raw.vignette.is_some());
    }

    #[test]
    fn unknown_opcodes_are_skipped_by_size() {
        let l = list(&[op(9, &[7u8; 33]), op(1, &warp_params(&[PLANE], [0.5, 0.5])), op(42, &[]), op(2, &[1u8; 100])]);
        let raw = parse_opcode_list(&l).expect("parsed");
        assert!(raw.warp.is_some() && raw.vignette.is_none());
        // Only unknown opcodes: parsed, nothing usable.
        let only = parse_opcode_list(&list(&[op(9, &[0u8; 8])])).expect("parsed");
        assert!(only.warp.is_none() && only.vignette.is_none());
    }

    #[test]
    fn truncated_and_malformed_lists_give_none() {
        let l = list(&[op(1, &warp_params(&[PLANE], [0.5, 0.5])), op(3, &vignette_params([0.1; 5], [0.5, 0.5]))]);
        for n in 0..l.len() {
            assert!(parse_opcode_list(&l[..n]).is_none(), "cut at {n} of {}", l.len());
        }
        assert!(parse_opcode_list(&l).is_some());
        // Count says more opcodes than there are.
        let mut more = l.clone();
        more[3] = 9;
        assert!(parse_opcode_list(&more).is_none());
        // Huge count and huge parameter size.
        assert!(parse_opcode_list(&be(u32::MAX)).is_none());
        let mut big = list(&[op(1, &[])]);
        big[16..20].copy_from_slice(&be(u32::MAX));
        assert!(parse_opcode_list(&big).is_none());
        // Plane count that does not match the parameter size.
        let mut p = warp_params(&[PLANE], [0.5, 0.5]);
        p[3] = 3;
        assert!(parse_opcode_list(&list(&[op(1, &p)])).is_none());
        // Two planes are well-formed but unusable; NaN numbers likewise.
        let two = list(&[op(1, &warp_params(&[PLANE, PLANE], [0.5, 0.5]))]);
        assert!(parse_opcode_list(&two).expect("parsed").warp.is_none());
        let nan = list(&[op(3, &vignette_params([f64::NAN; 5], [0.5, 0.5]))]);
        assert!(parse_opcode_list(&nan).expect("parsed").vignette.is_none());
    }

    // ---- orientation -----------------------------------------------------------

    /// The tangential displacement of the DNG warp (no radial part) at normalised (x, y).
    fn tangential(kt: [f64; 2], x: f64, y: f64) -> [f64; 2] {
        let r2 = x * x + y * y;
        [kt[0] * 2.0 * x * y + kt[1] * (r2 + 2.0 * x * x), kt[1] * 2.0 * x * y + kt[0] * (r2 + 2.0 * y * y)]
    }

    #[test]
    fn all_eight_orientations_move_the_off_centre_optical_centre_and_tangential_terms() {
        let (cx, cy) = (0.58, 0.46);
        let raw = parse_opcode_list(&list(&[
            op(1, &warp_params(&[[1.0, 0.0, 0.0, 0.0, 0.004, -0.007]], [cx, cy])),
            op(3, &vignette_params([-0.2, 0.0, 0.0, 0.0, 0.0], [cx, cy])),
        ]))
        .expect("parsed");
        // Position of the centre in the oriented image, by hand from the EXIF definition.
        let want: [(u16, [f64; 2]); 8] = [
            (1, [cx, cy]),
            (2, [1.0 - cx, cy]),
            (3, [1.0 - cx, 1.0 - cy]),
            (4, [cx, 1.0 - cy]),
            (5, [cy, cx]),
            (6, [1.0 - cy, cx]),
            (7, [1.0 - cy, 1.0 - cx]),
            (8, [cy, 1.0 - cx]),
        ];
        for (o, centre) in want {
            let g = Geometry { orientation: o, ..Geometry::plain(6000.0, 4000.0) };
            let l = to_upright(&raw, &g);
            let (w, v) = (l.warp.expect("warp"), l.vignette.expect("vignette"));
            assert!(close(w.center[0], centre[0]) && close(w.center[1], centre[1]), "warp centre, orientation {o}: {:?}", w.center);
            assert!(close(v.center[0], centre[0]) && close(v.center[1], centre[1]), "vignette centre, orientation {o}");
            assert_eq!(v.k[0], -0.2, "radial vignette terms are rotation invariant");

            // The displacement field is the original one carried through the orientation: pick a
            // source point, orient the point and the vector, and compare with the transformed
            // coefficients evaluated at the oriented point.
            let (sx, sy) = (0.31, -0.17);
            let src = tangential([0.004, -0.007], sx, sy);
            let (swap, fx, fy) = orientation_ops(o);
            let (mut px, mut py, mut vx, mut vy) = (sx, sy, src[0], src[1]);
            if swap {
                std::mem::swap(&mut px, &mut py);
                std::mem::swap(&mut vx, &mut vy);
            }
            if fx {
                px = -px;
                vx = -vx;
            }
            if fy {
                py = -py;
                vy = -vy;
            }
            let got = tangential([w.planes[0][4], w.planes[0][5]], px, py);
            assert!(close(got[0], vx) && close(got[1], vy), "tangential field, orientation {o}: {got:?} vs {:?}", [vx, vy]);
        }
        // Orientation 1 leaves the coefficients alone, 3 negates both tangential terms, 6 swaps and negates.
        let t = |o: u16| to_upright(&raw, &Geometry { orientation: o, ..Geometry::plain(6000.0, 4000.0) }).warp.expect("warp").planes[0];
        assert_eq!(&t(1)[4..], &[0.004, -0.007]);
        assert_eq!(&t(3)[4..], &[-0.004, 0.007]);
        assert_eq!(&t(6)[4..], &[-0.007, -0.004]);
        assert_eq!(&t(2)[4..], &[0.004, 0.007]);
    }

    #[test]
    fn crop_moves_the_centre_and_rescales_the_coefficients_exactly() {
        let raw = parse_opcode_list(&list(&[
            op(1, &warp_params(&[[1.0, 0.1, 0.05, 0.02, 0.003, -0.002]], [0.5, 0.5])),
            op(3, &vignette_params([0.3, -0.2, 0.1, 0.05, -0.02], [0.5, 0.5])),
        ]))
        .expect("parsed");
        // 100 x 60 area, crop 80 x 40 centred: centre stays 0.5/0.5; farthest corner 58.31 -> 44.72.
        let g = Geometry { area: (100.0, 60.0), crop: (10.0, 10.0, 80.0, 40.0), orientation: 1 };
        let l = to_upright(&raw, &g);
        let s = (40.0f64.powi(2) + 20.0f64.powi(2)).sqrt() / (50.0f64.powi(2) + 30.0f64.powi(2)).sqrt();
        let (w, v) = (l.warp.expect("warp"), l.vignette.expect("vignette"));
        assert!(close(w.center[0], 0.5) && close(w.center[1], 0.5));
        let p = w.planes[0];
        assert!(close(p[0], 1.0) && close(p[1], 0.1 * s * s) && close(p[2], 0.05 * s.powi(4)) && close(p[3], 0.02 * s.powi(6)));
        assert!(close(p[4], 0.003 * s) && close(p[5], -0.002 * s));
        for (i, k) in [0.3, -0.2, 0.1, 0.05, -0.02].iter().enumerate() {
            assert!(close(v.k[i], k * s.powi(2 * i as i32 + 2)), "k{i}");
        }
        // Equivalence of the radial polynomial: the same physical point gives the same factor.
        let r_crop = 0.6;
        let f = |q: &[f64; 6], r: f64| q[0] + q[1] * r * r + q[2] * r.powi(4) + q[3] * r.powi(6);
        assert!(close(f(&p, r_crop), f(&[1.0, 0.1, 0.05, 0.02, 0.0, 0.0], r_crop * s)));
        // Off-centre optical centre with an off-centre crop.
        let raw = parse_opcode_list(&list(&[op(3, &vignette_params([0.1; 5], [0.25, 0.75]))])).expect("parsed");
        let g = Geometry { area: (200.0, 100.0), crop: (20.0, 10.0, 160.0, 80.0), orientation: 1 };
        let c = to_upright(&raw, &g).vignette.expect("vignette").center;
        assert!(close(c[0], (50.0 - 20.0) / 160.0) && close(c[1], (75.0 - 10.0) / 80.0), "{c:?}");
    }

    // ---- files -----------------------------------------------------------------

    fn warp_list() -> Vec<u8> {
        list(&[op(1, &warp_params(&[PLANE], [0.55, 0.45])), op(3, &vignette_params([-0.1, 0.0, 0.0, 0.0, 0.0], [0.55, 0.45]))])
    }

    #[test]
    fn reads_dng_structures_in_both_byte_orders_and_both_ifd_layouts() {
        for be_container in [false, true] {
            for sub in [false, true] {
                let spec = Spec { be_container, sub, ..Spec::new(warp_list()) };
                let l = profile(&spec).unwrap_or_else(|| panic!("be={be_container} sub={sub}"));
                let w = l.warp.expect("warp");
                assert!(close(w.center[0], 0.55) && close(w.center[1], 0.45) && w.planes == vec![PLANE]);
                assert!(l.vignette.is_some());
            }
        }
    }

    #[test]
    fn file_orientation_active_area_and_crop_are_applied() {
        let spec = Spec {
            orientation: 6,
            size: (6048, 4032),
            active: Some([8, 16, 4024, 6032]), // top, left, bottom, right -> 6016 x 4016
            crop: Some(([8, 8], [6000, 4000])),
            ..Spec::new(warp_list())
        };
        let l = profile(&spec).expect("profile");
        let w = l.warp.expect("warp");
        // Centre in the area: (0.55 * 6016, 0.45 * 4016) = (3308.8, 1807.2); relative to the crop.
        let rel = [(3308.8 - 8.0) / 6000.0, (1807.2 - 8.0) / 4000.0];
        // Orientation 6 (rotate 90 CW): (1 - y, x).
        assert!(close(w.center[0], 1.0 - rel[1]) && close(w.center[1], rel[0]), "{:?}", w.center);
        // Swap + flip x: kt (0, 0) stays (0, 0); the radial terms are scaled by s only.
        assert!(close(w.planes[0][0], 1.0));
    }

    #[test]
    fn non_dng_and_missing_tag_and_garbage_give_none() {
        assert!(profile(&Spec { dng_version: false, ..Spec::new(warp_list()) }).is_none());
        assert!(profile(&Spec { opcodes: None, ..Spec::new(warp_list()) }).is_none());
        assert!(profile(&Spec::new(list(&[op(9, &[1, 2, 3])]))).is_none());
        assert!(read_profile(Cursor::new(Vec::<u8>::new())).is_none());
        assert!(read_profile(Cursor::new(b"not a tiff at all, just text".to_vec())).is_none());
        assert!(read_profile(Cursor::new(b"\x89PNG\r\n\x1a\n0000".to_vec())).is_none());
        assert!(read_lens_profile(Path::new("definitely/not/there.dng")).is_none());
    }

    #[test]
    fn damaged_files_never_panic() {
        let full = Spec { sub: true, ..Spec::new(warp_list()) }.build();
        assert!(read_profile(Cursor::new(full.clone())).is_some());
        for n in 0..full.len() {
            let _ = read_profile(Cursor::new(full[..n].to_vec()));
        }
        for i in 0..full.len() {
            for flip in [0xFFu8, 0x80, 0x01] {
                let mut v = full.clone();
                v[i] ^= flip;
                let _ = read_profile(Cursor::new(v));
            }
        }
        // Self-referencing IFD chain.
        let mut looped = full.clone();
        let n = u16::from_le_bytes([looped[8], looped[9]]) as usize;
        let next = 8 + 2 + n * 12;
        looped[next..next + 4].copy_from_slice(&8u32.to_le_bytes());
        let _ = read_profile(Cursor::new(looped));
    }
}
