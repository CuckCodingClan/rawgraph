//! Capture metadata that travels from a source file into an export (EXIF), the EXIF/TIFF
//! structures written for it, and reading of embedded XMP packets.
//!
//! No function here panics on file content; every parse is bounds-checked.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use rawgraph_core::MetaSelection;

use crate::IoError;

/// A GPS position as stored in EXIF.
///
/// Signed decimal degrees (north / east positive, so the EXIF `N/S` and `E/W` references are
/// folded into the sign) and an optional altitude in metres relative to sea level (negative =
/// below sea level, EXIF `GPSAltitudeRef` 1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GpsInfo {
    /// -90..=90, north positive.
    pub latitude: f64,
    /// -180..=180, east positive.
    pub longitude: f64,
    /// Metres above (positive) or below (negative) sea level.
    pub altitude_m: Option<f64>,
}

/// Everything optional; a field is written to the export only when present.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExportMetadata {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<u32>,
    /// Exposure time in seconds as a rational (numerator, denominator).
    pub exposure_time: Option<(u32, u32)>,
    pub f_number: Option<(u32, u32)>,
    /// Focal length in millimetres.
    pub focal_length: Option<(u32, u32)>,
    /// `YYYY:MM:DD HH:MM:SS` as in EXIF.
    pub datetime_original: Option<String>,
    pub artist: Option<String>,
    pub copyright: Option<String>,
    /// Defaults to "RawGraph" when the metadata is written and this is `None`; `Some("")` writes
    /// no Software tag at all (what [`ExportMetadata::select`] produces when the switch is off).
    pub software: Option<String>,
    /// GPS position (EXIF GPS IFD). Not written when latitude or longitude is not finite or out of
    /// range.
    pub gps: Option<GpsInfo>,
    /// Image description (EXIF `ImageDescription`, tag 270). Like every text field it is stored as
    /// UTF-8 bytes in the ASCII-typed entry (NUL-terminated): ASCII text is plain EXIF, other
    /// characters are the UTF-8 encoding that exiftool, macOS and Windows display correctly.
    /// Embedded NUL characters are dropped.
    pub description: Option<String>,
}

impl ExportMetadata {
    /// The part of the metadata that the switches allow: `camera` = make, model, lens; `exposure`
    /// = ISO, exposure time, f-number, focal length; `date` = capture date; `gps` = position;
    /// `author` = artist, copyright; `software` = the Software tag (a switched-off software tag is
    /// `Some("")`, which writes nothing instead of the default "RawGraph"). The description has
    /// no switch and is kept as it is; an override from the export settings is applied by the
    /// caller.
    pub fn select(&self, sel: &MetaSelection) -> ExportMetadata {
        let mut out = ExportMetadata { description: self.description.clone(), ..Default::default() };
        if sel.camera {
            out.make = self.make.clone();
            out.model = self.model.clone();
            out.lens = self.lens.clone();
        }
        if sel.exposure {
            out.iso = self.iso;
            out.exposure_time = self.exposure_time;
            out.f_number = self.f_number;
            out.focal_length = self.focal_length;
        }
        if sel.date {
            out.datetime_original = self.datetime_original.clone();
        }
        if sel.gps {
            out.gps = self.gps;
        }
        if sel.author {
            out.artist = self.artist.clone();
            out.copyright = self.copyright.clone();
        }
        out.software = if sel.software { self.software.clone() } else { Some(String::new()) };
        out
    }
}

// ---------------------------------------------------------------- reading metadata of a source

fn ascii(field: &exif::Field) -> Option<String> {
    if let exif::Value::Ascii(parts) = &field.value {
        let s = parts
            .iter()
            .map(|p| String::from_utf8_lossy(p).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string())
            .find(|s| !s.is_empty());
        return s;
    }
    None
}

fn rational(field: &exif::Field) -> Option<(u32, u32)> {
    match &field.value {
        exif::Value::Rational(v) => v.first().filter(|r| r.denom != 0 && r.num != 0).map(|r| (r.num, r.denom)),
        _ => None,
    }
}

/// Seconds / f-number / millimetres as a rational with a sensible denominator.
fn float_to_rational(v: f32, decimals_denominator: u32) -> Option<(u32, u32)> {
    if !v.is_finite() || v <= 0.0 {
        return None;
    }
    if v < 1.0 && decimals_denominator == 0 {
        // Exposure times: 1/n.
        let n = (1.0 / v).round();
        return (n >= 1.0 && n < u32::MAX as f32).then_some((1, n as u32));
    }
    let d = decimals_denominator.max(10);
    let n = (v * d as f32).round();
    (n >= 1.0 && n < u32::MAX as f32).then_some((n as u32, d))
}

fn from_exif(ex: &exif::Exif) -> ExportMetadata {
    let get = |tag| ex.get_field(tag, exif::In::PRIMARY);
    let iso = get(exif::Tag::PhotographicSensitivity)
        .and_then(|f| f.value.get_uint(0))
        .filter(|v| *v != 0 && *v != 65535)
        .or_else(|| get(exif::Tag::ISOSpeed).and_then(|f| f.value.get_uint(0)).filter(|v| *v != 0))
        .or_else(|| get(exif::Tag::RecommendedExposureIndex).and_then(|f| f.value.get_uint(0)).filter(|v| *v != 0));
    ExportMetadata {
        make: get(exif::Tag::Make).and_then(ascii),
        model: get(exif::Tag::Model).and_then(ascii),
        lens: get(exif::Tag::LensModel).and_then(ascii),
        iso,
        exposure_time: get(exif::Tag::ExposureTime).and_then(rational),
        f_number: get(exif::Tag::FNumber).and_then(rational),
        focal_length: get(exif::Tag::FocalLength).and_then(rational),
        datetime_original: get(exif::Tag::DateTimeOriginal).and_then(ascii).filter(|s| s.len() == 19),
        artist: get(exif::Tag::Artist).and_then(ascii),
        copyright: get(exif::Tag::Copyright).and_then(ascii),
        software: None,
        gps: gps_from_exif(ex),
        description: get(exif::Tag::ImageDescription).and_then(ascii),
    }
}

/// Degrees from the three (degrees, minutes, seconds) values of an EXIF GPS coordinate; fewer
/// values count as zero minutes / seconds.
fn dms_to_degrees(v: &[f64]) -> Option<f64> {
    let part = |i: usize| v.get(i).copied().unwrap_or(0.0);
    let deg = part(0) + part(1) / 60.0 + part(2) / 3600.0;
    (!v.is_empty() && deg.is_finite() && deg >= 0.0).then_some(deg)
}

/// Signed position from the raw EXIF parts. `None` for a missing / invalid reference, a value out
/// of range, or the exact position 0°/0° that cameras without a fix write.
pub(crate) fn gps_from_parts(
    lat: &[f64],
    lat_ref: Option<char>,
    lon: &[f64],
    lon_ref: Option<char>,
    altitude: Option<f64>,
    below_sea_level: bool,
) -> Option<GpsInfo> {
    let sign = |r: Option<char>, pos: char, neg: char| match r?.to_ascii_uppercase() {
        c if c == pos => Some(1.0),
        c if c == neg => Some(-1.0),
        _ => None,
    };
    let latitude = dms_to_degrees(lat)? * sign(lat_ref, 'N', 'S')?;
    let longitude = dms_to_degrees(lon)? * sign(lon_ref, 'E', 'W')?;
    if latitude.abs() > 90.0 || longitude.abs() > 180.0 || (latitude == 0.0 && longitude == 0.0) {
        return None;
    }
    let altitude_m = altitude.filter(|a| a.is_finite() && *a >= 0.0).map(|a| if below_sea_level { -a } else { a });
    Some(GpsInfo { latitude, longitude, altitude_m })
}

fn gps_from_exif(ex: &exif::Exif) -> Option<GpsInfo> {
    let get = |tag| ex.get_field(tag, exif::In::PRIMARY);
    let rationals = |tag| match &get(tag)?.value {
        exif::Value::Rational(v) if !v.is_empty() => Some(v.iter().map(|r| r.to_f64()).collect::<Vec<f64>>()),
        _ => None,
    };
    let reference = |tag| match &get(tag)?.value {
        exif::Value::Ascii(parts) => parts.first().and_then(|p| p.first()).map(|b| *b as char),
        _ => None,
    };
    let altitude = rationals(exif::Tag::GPSAltitude).and_then(|v| v.first().copied());
    let below = get(exif::Tag::GPSAltitudeRef).and_then(|f| f.value.get_uint(0)) == Some(1);
    gps_from_parts(
        &rationals(exif::Tag::GPSLatitude)?,
        reference(exif::Tag::GPSLatitudeRef),
        &rationals(exif::Tag::GPSLongitude)?,
        reference(exif::Tag::GPSLongitudeRef),
        altitude,
        below,
    )
}

/// Reads what can be carried over from a source file (RAW or rendered). Never fails the export:
/// problems give a partly empty struct. EXIF is read directly (TIFF-based RAW, JPEG, PNG, TIFF);
/// for RAW files whose container the EXIF reader does not know the RAW decoder fills the gaps.
pub fn read_export_metadata(path: &Path) -> ExportMetadata {
    let mut md = File::open(path)
        .ok()
        .and_then(|f| exif::Reader::new().read_from_container(&mut BufReader::new(f)).ok())
        .map(|ex| from_exif(&ex))
        .unwrap_or_default();
    if md.make.is_none() && md.model.is_none() && crate::source_kind(path) == Some(crate::SourceKind::Raw) {
        let r = crate::raw::export_metadata_fallback(path);
        md = ExportMetadata {
            make: md.make.or(r.make),
            model: md.model.or(r.model),
            lens: md.lens.or(r.lens),
            iso: md.iso.or(r.iso),
            exposure_time: md.exposure_time.or(r.exposure_time),
            f_number: md.f_number.or(r.f_number),
            focal_length: md.focal_length.or(r.focal_length),
            datetime_original: md.datetime_original.or(r.datetime_original),
            artist: md.artist.or(r.artist),
            copyright: md.copyright.or(r.copyright),
            software: None,
            gps: md.gps.or(r.gps),
            description: md.description.or(r.description),
        };
    }
    md
}

/// Helper for the RAW decoder fallback.
pub(crate) fn exposure_rational(seconds: f32) -> Option<(u32, u32)> {
    float_to_rational(seconds, 0)
}

pub(crate) fn tenth_rational(v: f32) -> Option<(u32, u32)> {
    float_to_rational(v, 10)
}

// ---------------------------------------------------------------- writing EXIF

const T_BYTE: u16 = 1;
const T_ASCII: u16 = 2;
const T_SHORT: u16 = 3;
const T_LONG: u16 = 4;
const T_RATIONAL: u16 = 5;
const T_UNDEFINED: u16 = 7;

/// An IFD entry: either one copied verbatim from an existing file (its out-of-line data stays
/// where it is) or a new one whose data is placed after the IFD.
enum Entry {
    Raw([u8; 12]),
    New { tag: u16, typ: u16, count: u32, data: Vec<u8> },
}

impl Entry {
    fn tag(&self) -> u16 {
        match self {
            Entry::Raw(e) => u16::from_le_bytes([e[0], e[1]]),
            Entry::New { tag, .. } => *tag,
        }
    }
}

/// An EXIF text entry (type ASCII, NUL-terminated). The text is stored as UTF-8: plain ASCII stays
/// valid EXIF, other characters use their UTF-8 bytes (readable by exiftool, macOS, Windows and
/// by [`read_export_metadata`]); embedded NULs are dropped.
fn text_entry(tag: u16, s: &str) -> Entry {
    let mut data: Vec<u8> = s.bytes().filter(|b| *b != 0).collect();
    data.push(0);
    Entry::New { tag, typ: T_ASCII, count: data.len() as u32, data }
}

fn short_entry(tag: u16, v: u16) -> Entry {
    Entry::New { tag, typ: T_SHORT, count: 1, data: v.to_le_bytes().to_vec() }
}

fn rationals_entry(tag: u16, values: &[(u32, u32)]) -> Entry {
    let mut data = Vec::with_capacity(values.len() * 8);
    for (n, d) in values {
        data.extend_from_slice(&n.to_le_bytes());
        data.extend_from_slice(&d.to_le_bytes());
    }
    Entry::New { tag, typ: T_RATIONAL, count: values.len() as u32, data }
}

fn rational_entry(tag: u16, v: (u32, u32)) -> Entry {
    rationals_entry(tag, &[v])
}

fn long_entry(tag: u16, v: u32) -> Entry {
    Entry::New { tag, typ: T_LONG, count: 1, data: v.to_le_bytes().to_vec() }
}

/// Serialises an IFD (little endian) that will start at byte `base` of the TIFF stream, followed by
/// the out-of-line data of its new entries. Entries are sorted by tag. The length of the result
/// does not depend on `base` and is always even.
fn serialize_ifd(mut entries: Vec<Entry>, base: u32) -> Vec<u8> {
    entries.sort_by_key(Entry::tag);
    let table = 2 + 12 * entries.len() + 4;
    let mut out = (entries.len() as u16).to_le_bytes().to_vec();
    let mut extra: Vec<u8> = Vec::new();
    for e in &entries {
        match e {
            Entry::Raw(raw) => out.extend_from_slice(raw),
            Entry::New { tag, typ, count, data } => {
                out.extend_from_slice(&tag.to_le_bytes());
                out.extend_from_slice(&typ.to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
                if data.len() <= 4 {
                    out.extend_from_slice(data);
                    out.extend_from_slice(&[0u8; 4][..4 - data.len()]);
                } else {
                    let off = base as usize + table + extra.len();
                    out.extend_from_slice(&(off as u32).to_le_bytes());
                    extra.extend_from_slice(data);
                    if extra.len() % 2 == 1 {
                        extra.push(0);
                    }
                }
            }
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    out.extend_from_slice(&extra);
    out
}

/// Degrees / minutes / seconds as three rationals (seconds with 1e-6 resolution); `value` >= 0.
fn dms_rationals(value: f64) -> [(u32, u32); 3] {
    const D: u32 = 1_000_000;
    let total = (value.abs() * 3600.0 * D as f64).round() as u64; // in 1e-6 arc seconds
    let secs_per_min = 60 * D as u64;
    let deg = total / (60 * secs_per_min);
    let min = (total / secs_per_min) % 60;
    let sec = total % secs_per_min;
    [(deg as u32, 1), (min as u32, 1), (sec as u32, D)]
}

/// The GPS IFD entries; empty when there is no (valid) position.
fn gps_entries(gps: &GpsInfo) -> Vec<Entry> {
    let valid = gps.latitude.is_finite()
        && gps.longitude.is_finite()
        && gps.latitude.abs() <= 90.0
        && gps.longitude.abs() <= 180.0;
    if !valid {
        return Vec::new();
    }
    let mut out = vec![
        Entry::New { tag: 0, typ: T_BYTE, count: 4, data: vec![2, 3, 0, 0] },
        text_entry(1, if gps.latitude < 0.0 { "S" } else { "N" }),
        rationals_entry(2, &dms_rationals(gps.latitude)),
        text_entry(3, if gps.longitude < 0.0 { "W" } else { "E" }),
        rationals_entry(4, &dms_rationals(gps.longitude)),
    ];
    if let Some(alt) = gps.altitude_m.filter(|a| a.is_finite() && a.abs() < 4.0e6) {
        out.push(Entry::New { tag: 5, typ: T_BYTE, count: 1, data: vec![u8::from(alt < 0.0)] });
        out.push(rational_entry(6, ((alt.abs() * 1000.0).round() as u32, 1000)));
    }
    out
}

/// The IFD0, Exif and GPS IFD entries for `md`. `srgb` is the Exif `ColorSpace` value: 1 for sRGB,
/// 0xFFFF (uncalibrated; the ICC profile says which space) for every other space.
fn build_entries(md: &ExportMetadata, srgb: bool) -> (Vec<Entry>, Vec<Entry>, Vec<Entry>) {
    let mut ifd0 = Vec::new();
    if let Some(v) = &md.description {
        ifd0.push(text_entry(270, v));
    }
    if let Some(v) = &md.make {
        ifd0.push(text_entry(271, v));
    }
    if let Some(v) = &md.model {
        ifd0.push(text_entry(272, v));
    }
    ifd0.push(short_entry(274, 1)); // Orientation: pixels are already upright
    match md.software.as_deref() {
        Some("") => {}
        other => ifd0.push(text_entry(305, other.unwrap_or("RawGraph"))),
    }
    if let Some(v) = &md.artist {
        ifd0.push(text_entry(315, v));
    }
    if let Some(v) = &md.copyright {
        ifd0.push(text_entry(33432, v));
    }

    let mut exif = Vec::new();
    if let Some(v) = md.exposure_time {
        exif.push(rational_entry(33434, v));
    }
    if let Some(v) = md.f_number {
        exif.push(rational_entry(33437, v));
    }
    if let Some(v) = md.iso {
        exif.push(short_entry(34855, v.min(65535) as u16));
    }
    exif.push(Entry::New { tag: 36864, typ: T_UNDEFINED, count: 4, data: b"0231".to_vec() });
    if let Some(v) = &md.datetime_original {
        exif.push(text_entry(36867, v));
    }
    if let Some(v) = md.focal_length {
        exif.push(rational_entry(37386, v));
    }
    exif.push(short_entry(40961, if srgb { 1 } else { 0xFFFF })); // ColorSpace
    if let Some(v) = &md.lens {
        exif.push(text_entry(42036, v));
    }
    let gps = md.gps.as_ref().map(gps_entries).unwrap_or_default();
    (ifd0, exif, gps)
}

/// A complete little-endian TIFF stream holding the EXIF data (what JPEG APP1 and PNG eXIf carry,
/// without the "Exif\0\0" prefix): IFD0, the Exif IFD and, when there is a position, the GPS IFD.
pub(crate) fn exif_blob(md: &ExportMetadata, srgb: bool) -> Vec<u8> {
    let (ifd0, exif, gps) = build_entries(md, srgb);
    let has_gps = !gps.is_empty();
    // IFD0 has a fixed size independent of the pointer values, so measure it first.
    let probe = serialize_ifd(with_pointers(ifd0_clone(&ifd0), 0, has_gps.then_some(0)), 8);
    let exif_off = 8 + probe.len() as u32;
    let exif_bytes = serialize_ifd(exif, exif_off);
    let gps_off = exif_off + exif_bytes.len() as u32;
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&serialize_ifd(with_pointers(ifd0, exif_off, has_gps.then_some(gps_off)), 8));
    out.extend_from_slice(&exif_bytes);
    if has_gps {
        out.extend_from_slice(&serialize_ifd(gps, gps_off));
    }
    out
}

fn ifd0_clone(v: &[Entry]) -> Vec<Entry> {
    v.iter()
        .map(|e| match e {
            Entry::Raw(r) => Entry::Raw(*r),
            Entry::New { tag, typ, count, data } => Entry::New { tag: *tag, typ: *typ, count: *count, data: data.clone() },
        })
        .collect()
}

fn with_pointers(mut ifd0: Vec<Entry>, exif_off: u32, gps_off: Option<u32>) -> Vec<Entry> {
    ifd0.push(long_entry(34665, exif_off));
    if let Some(off) = gps_off {
        ifd0.push(long_entry(34853, off));
    }
    ifd0
}

fn io_err(path: &Path, e: impl std::fmt::Display) -> IoError {
    IoError::Io(format!("{}: {e}", path.display()))
}

/// Adds the EXIF fields to a little-endian TIFF written by the `image` crate (which has no EXIF
/// support for TIFF): a new IFD0 (the old entries plus the new ones), the Exif IFD and the GPS IFD
/// are appended to the file and the header is pointed to the new IFD0. Pixel data and the ICC
/// profile stay as they are; the old IFD0 is left behind as unreferenced bytes (a few hundred).
pub(crate) fn tiff_add_exif(path: &Path, md: &ExportMetadata, srgb: bool) -> Result<(), IoError> {
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path).map_err(|e| io_err(path, e))?;
    let len = f.metadata().map_err(|e| io_err(path, e))?.len();
    if len >= u32::MAX as u64 - (1 << 20) {
        return Err(IoError::Io(format!("{}: TIFF too large for classic TIFF EXIF", path.display())));
    }
    let bad = || IoError::Io(format!("{}: unexpected TIFF structure", path.display()));
    let mut head = [0u8; 8];
    f.read_exact(&mut head).map_err(|e| io_err(path, e))?;
    if &head[..4] != b"II*\0" {
        return Err(bad());
    }
    let ifd_off = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as u64;
    f.seek(SeekFrom::Start(ifd_off)).map_err(|e| io_err(path, e))?;
    let mut cnt = [0u8; 2];
    f.read_exact(&mut cnt).map_err(|e| io_err(path, e))?;
    let n = u16::from_le_bytes(cnt) as usize;
    if n == 0 || n > 512 {
        return Err(bad());
    }
    let mut table = vec![0u8; n * 12];
    f.read_exact(&mut table).map_err(|e| io_err(path, e))?;

    let (new_ifd0, exif, gps) = build_entries(md, srgb);
    let replaced: Vec<u16> = new_ifd0.iter().map(Entry::tag).chain([34665, 34853]).collect();
    let mut entries: Vec<Entry> = table
        .as_chunks::<12>()
        .0
        .iter()
        .filter(|e| !replaced.contains(&u16::from_le_bytes([e[0], e[1]])))
        .map(|e| Entry::Raw(*e))
        .collect();
    entries.extend(new_ifd0);

    let start = len + (len % 2); // keep IFDs word aligned
    let exif_off = start as u32;
    let exif_bytes = serialize_ifd(exif, exif_off);
    let gps_off = exif_off + exif_bytes.len() as u32;
    let gps_bytes = if gps.is_empty() { Vec::new() } else { serialize_ifd(gps, gps_off) };
    let ifd0_off = gps_off + gps_bytes.len() as u32;
    entries.push(long_entry(34665, exif_off));
    if !gps_bytes.is_empty() {
        entries.push(long_entry(34853, gps_off));
    }
    let ifd0_bytes = serialize_ifd(entries, ifd0_off);

    f.seek(SeekFrom::Start(len)).map_err(|e| io_err(path, e))?;
    let mut tail = vec![0u8; (start - len) as usize];
    tail.extend_from_slice(&exif_bytes);
    tail.extend_from_slice(&gps_bytes);
    tail.extend_from_slice(&ifd0_bytes);
    f.write_all(&tail).map_err(|e| io_err(path, e))?;
    f.seek(SeekFrom::Start(4)).map_err(|e| io_err(path, e))?;
    f.write_all(&ifd0_off.to_le_bytes()).map_err(|e| io_err(path, e))?;
    f.flush().map_err(|e| io_err(path, e))
}

// ---------------------------------------------------------------- XMP

/// Upper bound for an XMP packet we are willing to read.
const MAX_XMP: u64 = 64 << 20;

fn rd<R: Read>(r: &mut R, n: usize) -> Option<Vec<u8>> {
    let mut b = vec![0u8; n];
    r.read_exact(&mut b).ok()?;
    Some(b)
}

fn xmp_from_bytes(b: Vec<u8>) -> Option<String> {
    let s = String::from_utf8_lossy(&b);
    let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace() || c == '\u{feff}');
    (!s.is_empty()).then(|| s.to_string())
}

fn xmp_jpeg(f: &mut BufReader<File>) -> Option<String> {
    const NS: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
    f.seek(SeekFrom::Start(2)).ok()?;
    loop {
        // Marker: 0xFF, optional fill bytes, code.
        let mut b = [0u8; 1];
        f.read_exact(&mut b).ok()?;
        if b[0] != 0xFF {
            return None;
        }
        let mut code = 0xFFu8;
        while code == 0xFF {
            f.read_exact(&mut b).ok()?;
            code = b[0];
        }
        match code {
            0xD9 | 0xDA => return None, // EOI or start of scan: no more metadata segments
            0x01 | 0xD0..=0xD7 | 0xD8 => continue,
            _ => {}
        }
        let len = u16::from_be_bytes(rd(f, 2)?.try_into().ok()?) as usize;
        if len < 2 {
            return None;
        }
        let body = len - 2;
        if code == 0xE1 && body >= NS.len() {
            let head = rd(f, NS.len())?;
            if head == NS {
                return xmp_from_bytes(rd(f, body - NS.len())?);
            }
            f.seek(SeekFrom::Current((body - NS.len()) as i64)).ok()?;
        } else {
            f.seek(SeekFrom::Current(body as i64)).ok()?;
        }
    }
}

fn xmp_png(f: &mut BufReader<File>) -> Option<String> {
    f.seek(SeekFrom::Start(8)).ok()?;
    loop {
        let head = rd(f, 8)?;
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as u64;
        let kind = &head[4..8];
        if kind == b"IEND" {
            return None;
        }
        if kind == b"iTXt" && len <= MAX_XMP {
            let data = rd(f, len as usize)?;
            const KEY: &[u8] = b"XML:com.adobe.xmp\0";
            if data.starts_with(KEY) && data.len() > KEY.len() + 2 {
                let (compressed, rest) = (data[KEY.len()], &data[KEY.len() + 2..]);
                if compressed != 0 {
                    return None; // zlib-compressed XMP is not supported (and not used by XMP writers)
                }
                // language tag, NUL, translated keyword, NUL, text
                let mut parts = rest.splitn(3, |c| *c == 0);
                parts.next()?;
                parts.next()?;
                return xmp_from_bytes(parts.next()?.to_vec());
            }
            f.seek(SeekFrom::Current(4)).ok()?; // CRC
        } else {
            f.seek(SeekFrom::Current(len as i64 + 4)).ok()?;
        }
    }
}

fn xmp_tiff(f: &mut BufReader<File>, le: bool) -> Option<String> {
    let u16_at = |b: &[u8]| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) };
    let u32_at = |b: &[u8]| if le { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) };
    f.seek(SeekFrom::Start(4)).ok()?;
    let ifd = u32_at(&rd(f, 4)?) as u64;
    f.seek(SeekFrom::Start(ifd)).ok()?;
    let n = u16_at(&rd(f, 2)?) as usize;
    if n == 0 || n > 4096 {
        return None;
    }
    let table = rd(f, n * 12)?;
    let e = table.as_chunks::<12>().0.iter().find(|e| u16_at(&e[0..2]) == 700)?;
    let (typ, count) = (u16_at(&e[2..4]), u32_at(&e[4..8]) as u64);
    if !matches!(typ, 1 | 2 | 7) || count <= 4 || count > MAX_XMP {
        return None;
    }
    f.seek(SeekFrom::Start(u32_at(&e[8..12]) as u64)).ok()?;
    xmp_from_bytes(rd(f, count as usize)?)
}

/// The XMP packet embedded in a file: tag 700 of a TIFF / DNG / TIFF-based RAW (ARW, NEF, ...), the
/// APP1 segment "http://ns.adobe.com/xap/1.0/" of a JPEG, or the uncompressed iTXt chunk
/// "XML:com.adobe.xmp" of a PNG. Container recognition is by content, not by extension. `None`
/// when there is no packet or the file cannot be read; only the headers and the packet are read,
/// never the pixel data. (Not read: extended XMP in further JPEG segments, ISO-BMFF containers such
/// as CR3.)
pub fn read_xmp_packet(path: &Path) -> Option<String> {
    let mut f = BufReader::new(File::open(path).ok()?);
    let magic = rd(&mut f, 8)?;
    match &magic[..] {
        [0xFF, 0xD8, ..] => xmp_jpeg(&mut f),
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] => xmp_png(&mut f),
        [b'I', b'I', 42, 0, ..] => xmp_tiff(&mut f, true),
        [b'M', b'M', 0, 42, ..] => xmp_tiff(&mut f, false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ExportMetadata {
        ExportMetadata {
            make: Some("SONY".into()),
            model: Some("ILCE-7M4".into()),
            lens: Some("FE 35mm F1.4 GM".into()),
            iso: Some(640),
            exposure_time: Some((1, 250)),
            f_number: Some((28, 10)),
            focal_length: Some((35, 1)),
            datetime_original: Some("2026:09:26 14:03:09".into()),
            artist: Some("A. Photographer".into()),
            copyright: Some("(c) Test".into()),
            software: None,
            gps: None,
            description: None,
        }
    }

    #[test]
    fn exif_blob_parses_with_kamadak() {
        let blob = exif_blob(&sample(), true);
        let (fields, _) = exif::parse_exif(&blob).expect("valid exif");
        let get = |t: exif::Tag| fields.iter().find(|f| f.tag == t);
        assert_eq!(get(exif::Tag::Make).and_then(ascii).as_deref(), Some("SONY"));
        assert_eq!(get(exif::Tag::Model).and_then(ascii).as_deref(), Some("ILCE-7M4"));
        assert_eq!(get(exif::Tag::Software).and_then(ascii).as_deref(), Some("RawGraph"));
        assert_eq!(get(exif::Tag::Artist).and_then(ascii).as_deref(), Some("A. Photographer"));
        assert_eq!(get(exif::Tag::Copyright).and_then(ascii).as_deref(), Some("(c) Test"));
        assert_eq!(get(exif::Tag::LensModel).and_then(ascii).as_deref(), Some("FE 35mm F1.4 GM"));
        assert_eq!(get(exif::Tag::DateTimeOriginal).and_then(ascii).as_deref(), Some("2026:09:26 14:03:09"));
        assert_eq!(get(exif::Tag::Orientation).and_then(|f| f.value.get_uint(0)), Some(1));
        assert_eq!(get(exif::Tag::PhotographicSensitivity).and_then(|f| f.value.get_uint(0)), Some(640));
        assert_eq!(get(exif::Tag::ExposureTime).and_then(rational), Some((1, 250)));
        assert_eq!(get(exif::Tag::FNumber).and_then(rational), Some((28, 10)));
        assert_eq!(get(exif::Tag::FocalLength).and_then(rational), Some((35, 1)));
    }

    #[test]
    fn empty_metadata_still_has_orientation_and_software() {
        let blob = exif_blob(&ExportMetadata::default(), true);
        let (fields, _) = exif::parse_exif(&blob).expect("valid exif");
        assert!(fields.iter().any(|f| f.tag == exif::Tag::Orientation));
        assert!(fields.iter().any(|f| f.tag == exif::Tag::Software));
        assert!(!fields.iter().any(|f| f.tag == exif::Tag::Make));
    }

    #[test]
    fn rational_helpers() {
        assert_eq!(exposure_rational(0.004), Some((1, 250)));
        assert_eq!(exposure_rational(2.0), Some((20, 10)));
        assert_eq!(tenth_rational(2.8), Some((28, 10)));
        assert_eq!(exposure_rational(0.0), None);
        assert_eq!(tenth_rational(f32::NAN), None);
    }
}
