//! Colour lookup tables: the Adobe / Resolve `.cube` text format, a cache of loaded files and the
//! interpolation (tetrahedral for 3D, linear for 1D).
//!
//! Supported keywords: `TITLE`, `DOMAIN_MIN`, `DOMAIN_MAX`, `LUT_1D_SIZE` (2..=65536),
//! `LUT_3D_SIZE` (2..=65), the Resolve per-table `LUT_1D_INPUT_RANGE` / `LUT_3D_INPUT_RANGE`
//! (two numbers, used for all three channels) and the video range markers
//! `LUT_IN_VIDEO_RANGE` / `LUT_OUT_VIDEO_RANGE` (ignored). A file with BOTH a `LUT_1D_SIZE` and a
//! `LUT_3D_SIZE` holds the 1D rows first and the 3D rows after them; the 1D table is then a
//! per-channel shaper applied before the 3D cube (Resolve writes such files). Comments start
//! with `#`, line ends may be LF or CRLF, numbers may use scientific notation. 3D rows run with
//! red fastest, then green, then blue (the `.cube` order).
//!
//! Nothing here panics on bad input: every problem is a [`LutError`] with the line number.

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Largest `LUT_3D_SIZE`.
pub const MAX_3D_SIZE: usize = 65;
/// Largest `LUT_1D_SIZE`.
pub const MAX_1D_SIZE: usize = 65_536;
/// Largest file that is read (a 65^3 cube is about 8 MB of text).
pub const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
/// Loaded files that stay in the cache.
const CACHE_ENTRIES: usize = 16;

/// What goes wrong when a LUT file is read or parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LutError {
    /// 1-based line of the file, when the problem belongs to one.
    pub line: Option<usize>,
    pub message: String,
}

impl LutError {
    fn at(line: usize, message: impl Into<String>) -> Self {
        Self { line: Some(line), message: message.into() }
    }

    fn general(message: impl Into<String>) -> Self {
        Self { line: None, message: message.into() }
    }
}

impl fmt::Display for LutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for LutError {}

/// The space the table values are defined in, as far as the pixel pipeline needs to know it.
/// (The node's "Display" input space wraps a view transform around [`LutSpace::Linear`]-style
/// direct sampling and is handled by the engine.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LutSpace {
    /// The working values (linear ACEScg) go into the table unchanged.
    Linear,
    /// Linear Rec.709 primaries, sRGB encoded and clamped to 0..1.
    Srgb709,
    /// ACEScct (log) on the AP1 primaries.
    AcesCct,
}

/// One table: `size` entries per axis (1D: `size` rows; 3D: `size`^3 rows), RGB per row.
#[derive(Clone, Debug)]
pub struct LutTable {
    pub size: usize,
    pub domain_min: [f32; 3],
    pub domain_max: [f32; 3],
    /// `1 / (domain_max - domain_min)`, computed once so CPU and GPU scale identically.
    pub inv_range: [f32; 3],
    /// RGB triples. 3D: red index fastest, then green, then blue.
    pub data: Vec<f32>,
}

/// A parsed LUT: a 1D table, a 3D cube or both (the 1D table is then applied first).
#[derive(Clone, Debug)]
pub struct Lut {
    pub title: Option<String>,
    pub lut1d: Option<LutTable>,
    pub lut3d: Option<LutTable>,
    hash: u64,
}

impl PartialEq for Lut {
    /// Same content (a 64-bit content hash plus the sizes), so a tail plan compares cheaply.
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
            && self.lut1d.as_ref().map(|t| t.size) == other.lut1d.as_ref().map(|t| t.size)
            && self.lut3d.as_ref().map(|t| t.size) == other.lut3d.as_ref().map(|t| t.size)
    }
}

impl LutTable {
    fn new(size: usize, domain_min: [f32; 3], domain_max: [f32; 3], data: Vec<f32>) -> Self {
        let inv_range = std::array::from_fn(|c| 1.0 / (domain_max[c] - domain_min[c]));
        Self { size, domain_min, domain_max, inv_range, data }
    }

    /// Position of `v` on channel `c` in table units, clamped to the domain: 0..=size-1.
    /// NaN counts as the domain minimum.
    #[inline]
    fn position(&self, c: usize, v: f32) -> f32 {
        let t = (v - self.domain_min[c]) * self.inv_range[c];
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        t * (self.size - 1) as f32
    }

    #[inline]
    fn row(&self, i: usize) -> [f32; 3] {
        [self.data[i * 3], self.data[i * 3 + 1], self.data[i * 3 + 2]]
    }

    /// Linear interpolation, each channel in its own column.
    fn sample_1d(&self, rgb: [f32; 3]) -> [f32; 3] {
        let mut out = [0.0f32; 3];
        for c in 0..3 {
            let t = self.position(c, rgb[c]);
            let i = (t as usize).min(self.size - 2);
            let f = t - i as f32;
            let a = self.data[i * 3 + c];
            let b = self.data[(i + 1) * 3 + c];
            out[c] = a + (b - a) * f;
        }
        out
    }

    /// Tetrahedral interpolation (the cube is cut into six tetrahedra along its main diagonal).
    fn sample_3d(&self, rgb: [f32; 3]) -> [f32; 3] {
        let n = self.size;
        let t = [self.position(0, rgb[0]), self.position(1, rgb[1]), self.position(2, rgb[2])];
        let i0: [usize; 3] = std::array::from_fn(|c| (t[c] as usize).min(n - 2));
        let f: [f32; 3] = std::array::from_fn(|c| t[c] - i0[c] as f32);
        let base = i0[0] + i0[1] * n + i0[2] * n * n;
        let (dr, dg, db) = (1usize, n, n * n);
        let at = |off: usize| self.row(base + off);
        let c000 = at(0);
        let c111 = at(dr + dg + db);
        let (fr, fg, fb) = (f[0], f[1], f[2]);
        // The path from c000 to c111 runs along the three axes, the axis with the largest
        // fraction first: (weight1, offset1, weight2, offset2, weight3).
        let (w1, o1, w2, o2, w3) = if fr >= fg {
            if fg >= fb {
                (fr, dr, fg, dg, fb)
            } else if fr >= fb {
                (fr, dr, fb, db, fg)
            } else {
                (fb, db, fr, dr, fg)
            }
        } else if fb >= fg {
            (fb, db, fg, dg, fr)
        } else if fb >= fr {
            (fg, dg, fb, db, fr)
        } else {
            (fg, dg, fr, dr, fb)
        };
        let (v1, v2) = (at(o1), at(o1 + o2));
        std::array::from_fn(|k| c000[k] + w1 * (v1[k] - c000[k]) + w2 * (v2[k] - v1[k]) + w3 * (c111[k] - v2[k]))
    }
}
impl Lut {
    fn new(title: Option<String>, lut1d: Option<LutTable>, lut3d: Option<LutTable>) -> Self {
        let mut h = DefaultHasher::new();
        for t in [&lut1d, &lut3d] {
            match t {
                None => 0u8.hash(&mut h),
                Some(t) => {
                    1u8.hash(&mut h);
                    t.size.hash(&mut h);
                    t.domain_min.map(f32::to_bits).hash(&mut h);
                    t.domain_max.map(f32::to_bits).hash(&mut h);
                    for v in &t.data {
                        h.write_u32(v.to_bits());
                    }
                }
            }
        }
        Self { title, lut1d, lut3d, hash: h.finish() }
    }

    /// Hash of the table content (sizes, domains, every value).
    pub fn content_hash(&self) -> u64 {
        self.hash
    }

    /// The table output for one RGB triple: the 1D table (if any), then the cube (if any). Input
    /// outside a table's domain is clamped to it; NaN counts as the domain minimum.
    pub fn sample(&self, rgb: [f32; 3]) -> [f32; 3] {
        let mut v = rgb;
        if let Some(t) = &self.lut1d {
            v = t.sample_1d(v);
        }
        if let Some(t) = &self.lut3d {
            v = t.sample_3d(v);
        }
        v
    }

    /// Short description for the inspector, e.g. "3D 33", "1D 1024" or "1D 4096 + 3D 17".
    pub fn describe(&self) -> String {
        match (&self.lut1d, &self.lut3d) {
            (Some(a), Some(b)) => format!("1D {} + 3D {}", a.size, b.size),
            (Some(a), None) => format!("1D {}", a.size),
            (None, Some(b)) => format!("3D {}", b.size),
            (None, None) => "empty".to_owned(),
        }
    }
}

/// ACEScg (AP1, linear) -> ACEScct.
pub fn acescct_encode(x: f32) -> f32 {
    if x <= 0.007_812_5 {
        10.540_238 * x + 0.072_905_53
    } else {
        (x.log2() + 9.72) / 17.52
    }
}

/// ACEScct -> ACEScg (AP1, linear).
pub fn acescct_decode(y: f32) -> f32 {
    if y <= 0.155_251_14 {
        (y - 0.072_905_53) / 10.540_238
    } else {
        (y * 17.52 - 9.72).exp2()
    }
}

// ---- parser --------------------------------------------------------------------------------

fn parse_floats(line_no: usize, what: &str, tokens: &[&str], want: usize) -> Result<Vec<f32>, LutError> {
    if tokens.len() != want {
        return Err(LutError::at(line_no, format!("{what} needs {want} number(s), found {}", tokens.len())));
    }
    tokens
        .iter()
        .map(|t| match t.parse::<f32>() {
            Ok(v) if v.is_finite() => Ok(v),
            Ok(_) => Err(LutError::at(line_no, format!("{what}: \"{t}\" is not a finite number"))),
            Err(_) => Err(LutError::at(line_no, format!("{what}: \"{t}\" is not a number"))),
        })
        .collect()
}

fn parse_size(line_no: usize, keyword: &str, tokens: &[&str], max: usize) -> Result<usize, LutError> {
    if tokens.len() != 1 {
        return Err(LutError::at(line_no, format!("{keyword} needs one integer")));
    }
    let n: usize = tokens[0]
        .parse()
        .map_err(|_| LutError::at(line_no, format!("{keyword}: \"{}\" is not a whole number", tokens[0])))?;
    if !(2..=max).contains(&n) {
        return Err(LutError::at(line_no, format!("{keyword} {n} is outside 2..={max}")));
    }
    Ok(n)
}

fn is_data_line(first: &str) -> bool {
    first.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '-' | '+' | '.'))
}

/// Parses the text of a `.cube` file.
pub fn parse_cube(text: &str) -> Result<Lut, LutError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut title: Option<String> = None;
    let (mut dmin_set, mut dmax_set) = (None::<[f32; 3]>, None::<[f32; 3]>);
    let (mut size1, mut size3): (Option<usize>, Option<usize>) = (None, None);
    let (mut range1, mut range3): (Option<[f32; 2]>, Option<[f32; 2]>) = (None, None);
    let mut data: Vec<f32> = Vec::new();
    let mut rows = 0usize;
    let mut expected = 0usize;
    let mut sized = false;
    let mut last_line = 0usize;

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        last_line = line_no;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let first = line.split_whitespace().next().unwrap_or("");
        if is_data_line(first) {
            let body = line.split('#').next().unwrap_or("");
            let tokens: Vec<&str> = body.split_whitespace().collect();
            if !sized {
                return Err(LutError::at(line_no, "data row before LUT_1D_SIZE / LUT_3D_SIZE"));
            }
            if rows >= expected {
                return Err(LutError::at(line_no, format!("more data rows than the {expected} the sizes declare")));
            }
            let v = parse_floats(line_no, "data row", &tokens, 3)?;
            data.extend_from_slice(&v);
            rows += 1;
            continue;
        }
        if rows > 0 {
            return Err(LutError::at(line_no, format!("keyword \"{first}\" after the data rows started")));
        }
        if first == "TITLE" {
            let rest = line[first.len()..].trim();
            let rest = rest.strip_prefix('"').map_or(rest, |r| r.rsplit_once('"').map_or(r, |(t, _)| t));
            title = Some(rest.to_owned());
            continue;
        }
        let body = line.split('#').next().unwrap_or("");
        let mut parts = body.split_whitespace();
        let keyword = parts.next().unwrap_or("");
        let tokens: Vec<&str> = parts.collect();
        match keyword {
            "DOMAIN_MIN" => {
                let v = parse_floats(line_no, "DOMAIN_MIN", &tokens, 3)?;
                dmin_set = Some([v[0], v[1], v[2]]);
            }
            "DOMAIN_MAX" => {
                let v = parse_floats(line_no, "DOMAIN_MAX", &tokens, 3)?;
                dmax_set = Some([v[0], v[1], v[2]]);
            }
            "LUT_1D_INPUT_RANGE" => {
                let v = parse_floats(line_no, "LUT_1D_INPUT_RANGE", &tokens, 2)?;
                range1 = Some([v[0], v[1]]);
            }
            "LUT_3D_INPUT_RANGE" => {
                let v = parse_floats(line_no, "LUT_3D_INPUT_RANGE", &tokens, 2)?;
                range3 = Some([v[0], v[1]]);
            }
            "LUT_1D_SIZE" => {
                if size1.is_some() {
                    return Err(LutError::at(line_no, "LUT_1D_SIZE is given twice"));
                }
                size1 = Some(parse_size(line_no, "LUT_1D_SIZE", &tokens, MAX_1D_SIZE)?);
            }
            "LUT_3D_SIZE" => {
                if size3.is_some() {
                    return Err(LutError::at(line_no, "LUT_3D_SIZE is given twice"));
                }
                size3 = Some(parse_size(line_no, "LUT_3D_SIZE", &tokens, MAX_3D_SIZE)?);
            }
            "LUT_IN_VIDEO_RANGE" | "LUT_OUT_VIDEO_RANGE" => {}
            other => return Err(LutError::at(line_no, format!("unknown keyword \"{other}\""))),
        }
        if size1.is_some() || size3.is_some() {
            sized = true;
            expected = size1.unwrap_or(0) + size3.map_or(0, |n| n * n * n);
            if data.capacity() == 0 {
                data.reserve_exact(expected * 3);
            }
        }
    }

    if size1.is_none() && size3.is_none() {
        return Err(LutError::general("no LUT_1D_SIZE or LUT_3D_SIZE in the file"));
    }
    if rows < expected {
        return Err(LutError::at(last_line.max(1), format!("expected {expected} data rows, found {rows}")));
    }

    let domain_for = |range: Option<[f32; 2]>| -> Result<([f32; 3], [f32; 3]), LutError> {
        let (lo, hi) = match range {
            Some([lo, hi]) => ([lo; 3], [hi; 3]),
            None => (dmin_set.unwrap_or([0.0; 3]), dmax_set.unwrap_or([1.0; 3])),
        };
        if (0..3).any(|c| lo[c] >= hi[c]) {
            return Err(LutError::general("the domain minimum must be below the maximum on every channel"));
        }
        Ok((lo, hi))
    };

    let n1 = size1.unwrap_or(0);
    let split = n1 * 3;
    let lut1d = match size1 {
        Some(n) => {
            let (lo, hi) = domain_for(range1)?;
            Some(LutTable::new(n, lo, hi, data[..split].to_vec()))
        }
        None => None,
    };
    let lut3d = match size3 {
        Some(n) => {
            let (lo, hi) = domain_for(range3)?;
            Some(LutTable::new(n, lo, hi, data[split..].to_vec()))
        }
        None => None,
    };
    Ok(Lut::new(title, lut1d, lut3d))
}

/// Reads and parses a file without using the cache.
pub fn load_file(path: &Path) -> Result<Lut, LutError> {
    let meta = std::fs::metadata(path).map_err(|e| LutError::general(format!("cannot read the file: {e}")))?;
    if !meta.is_file() {
        return Err(LutError::general("not a file"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(LutError::general(format!("the file is larger than {} MB", MAX_FILE_BYTES / (1024 * 1024))));
    }
    let bytes = std::fs::read(path).map_err(|e| LutError::general(format!("cannot read the file: {e}")))?;
    parse_cube(&String::from_utf8_lossy(&bytes))
}

// ---- cache ---------------------------------------------------------------------------------

/// Identity of a file's content as far as the file system tells: size and modification time.
pub type FileStamp = (u64, Option<SystemTime>);

/// Size and modification time of `path`; `None` if it cannot be read.
pub fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()))
}

type CacheEntry = (PathBuf, FileStamp, Arc<Lut>);

fn cache() -> &'static Mutex<Vec<CacheEntry>> {
    static CACHE: Mutex<Vec<CacheEntry>> = Mutex::new(Vec::new());
    &CACHE
}

/// Loads `path` through a process-wide cache keyed by path, file size and modification time:
/// an unchanged file is parsed once, a changed one is read again. Errors are not cached.
pub fn load_cached(path: &Path) -> Result<Arc<Lut>, LutError> {
    let Some(stamp) = file_stamp(path) else {
        // The metadata call gives the reason (missing file, no permission, ...).
        return Err(match std::fs::metadata(path) {
            Err(e) => LutError::general(format!("cannot read the file: {e}")),
            Ok(_) => LutError::general("cannot read the file"),
        });
    };
    {
        let cache = cache().lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, _, lut)) = cache.iter().find(|(p, s, _)| p == path && *s == stamp) {
            return Ok(Arc::clone(lut));
        }
    }
    let lut = Arc::new(load_file(path)?);
    let mut cache = cache().lock().unwrap_or_else(|p| p.into_inner());
    cache.retain(|(p, _, _)| p != path);
    if cache.len() >= CACHE_ENTRIES {
        cache.remove(0);
    }
    cache.push((path.to_path_buf(), stamp, Arc::clone(&lut)));
    Ok(lut)
}

/// Drops `path` from the cache so the next load reads the file again ("Reload").
pub fn forget(path: &Path) {
    cache().lock().unwrap_or_else(|p| p.into_inner()).retain(|(p, _, _)| p != path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_3d(n: usize) -> String {
        let mut s = format!("TITLE \"id\"\nLUT_3D_SIZE {n}\n");
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let d = (n - 1) as f32;
                    s.push_str(&format!("{} {} {}\n", r as f32 / d, g as f32 / d, b as f32 / d));
                }
            }
        }
        s
    }

    fn err(text: &str) -> LutError {
        parse_cube(text).expect_err("must fail")
    }

    #[test]
    fn parses_a_3d_cube_with_title_comments_crlf_and_scientific_notation() {
        let mut text = String::from("\u{feff}# made by hand\r\nTITLE \"My look # one\"\r\nDOMAIN_MIN 0 0 0\r\nDOMAIN_MAX 1.0 1.0 1e0\r\nLUT_3D_SIZE 2\r\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    text.push_str(&format!("{}.0E+0 {} {}  # row\r\n", r, g, b as f32 * 5e-1));
                }
            }
        }
        let lut = parse_cube(&text).unwrap();
        assert_eq!(lut.title.as_deref(), Some("My look # one"));
        let cube = lut.lut3d.as_ref().unwrap();
        assert_eq!((cube.size, cube.data.len()), (2, 24));
        assert_eq!(cube.row(7), [1.0, 1.0, 0.5]);
        assert_eq!(lut.describe(), "3D 2");
    }

    #[test]
    fn parses_a_1d_table_and_a_domain() {
        let lut = parse_cube("LUT_1D_SIZE 3\nDOMAIN_MIN -1 -1 -1\nDOMAIN_MAX 3 3 3\n0 0 0\n0.5 0.25 1\n1 1 1\n").unwrap();
        let t = lut.lut1d.as_ref().unwrap();
        assert_eq!((t.size, t.domain_min, t.domain_max), (3, [-1.0; 3], [3.0; 3]));
        // domain -1..3 -> input 1.0 is the middle row.
        assert_eq!(lut.sample([1.0, 1.0, 1.0]), [0.5, 0.25, 1.0]);
        // halfway between row 0 and row 1 (input 0.0 is at 1/4 of the domain = table position 0.5).
        assert_eq!(lut.sample([0.0, 0.0, 0.0]), [0.25, 0.125, 0.5]);
    }

    #[test]
    fn parses_both_tables_and_the_resolve_input_ranges() {
        let mut text = String::from("LUT_1D_SIZE 2\nLUT_3D_SIZE 2\nLUT_1D_INPUT_RANGE 0 2\nLUT_IN_VIDEO_RANGE\n0 0 0\n1 1 1\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    text.push_str(&format!("{r} {g} {b}\n"));
                }
            }
        }
        let lut = parse_cube(&text).unwrap();
        assert_eq!(lut.describe(), "1D 2 + 3D 2");
        assert_eq!(lut.lut1d.as_ref().unwrap().domain_max, [2.0; 3]);
        // Shaper: input 1.0 -> 0.5 (range 0..2), the identity cube keeps it.
        let v = lut.sample([1.0, 2.0, 0.0]);
        assert!((v[0] - 0.5).abs() < 1e-6 && (v[1] - 1.0).abs() < 1e-6 && v[2].abs() < 1e-6, "{v:?}");
    }

    #[test]
    fn identity_cube_samples_back_the_input() {
        let lut = parse_cube(&identity_3d(5)).unwrap();
        for v in [[0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [0.13, 0.77, 0.5], [0.99, 0.01, 0.4]] {
            let o = lut.sample(v);
            for c in 0..3 {
                assert!((o[c] - v[c]).abs() < 1e-6, "{v:?} -> {o:?}");
            }
        }
    }

    #[test]
    fn out_of_domain_input_is_clamped_and_nan_is_the_minimum() {
        let lut = parse_cube(&identity_3d(3)).unwrap();
        assert_eq!(lut.sample([-4.0, 7.0, f32::INFINITY]), [0.0, 1.0, 1.0]);
        assert_eq!(lut.sample([f32::NAN, f32::NEG_INFINITY, 0.5]), [0.0, 0.0, 0.5]);
    }

    #[test]
    fn malformed_files_report_the_line() {
        let cases: Vec<(&str, usize, &str)> = vec![
            ("LUT_3D_SIZE 2\n0 0 0\n0 0\n", 3, "3 number"),
            ("LUT_3D_SIZE 2\n0 0 x\n", 2, "not a number"),
            ("LUT_3D_SIZE 2\n0 0 nan\n", 2, "finite"),
            ("LUT_3D_SIZE 1\n", 1, "outside"),
            ("LUT_3D_SIZE 66\n", 1, "outside"),
            ("LUT_1D_SIZE 65537\n", 1, "outside"),
            ("LUT_3D_SIZE 2.5\n", 1, "whole number"),
            ("# c\nBOGUS 1\n", 2, "unknown keyword"),
            ("0 0 0\nLUT_3D_SIZE 2\n", 1, "before"),
            ("LUT_1D_SIZE 2\n0 0 0\n1 1 1\nDOMAIN_MIN 0 0 0\n", 4, "after the data"),
            ("LUT_1D_SIZE 2\n0 0 0\n1 1 1\n2 2 2\n", 4, "more data rows"),
            ("LUT_1D_SIZE 2\n0 0 0\n", 2, "expected 2 data rows, found 1"),
            ("LUT_1D_SIZE 2\nLUT_1D_SIZE 3\n", 2, "twice"),
            ("DOMAIN_MIN 0 0\n", 1, "3 number"),
        ];
        for (text, line, needle) in cases {
            let e = err(text);
            assert_eq!(e.line, Some(line), "{text:?}: {e}");
            assert!(e.message.contains(needle), "{text:?}: {e}");
            assert!(e.to_string().starts_with(&format!("line {line}: ")));
        }
        let e = err("TITLE \"x\"\n");
        assert_eq!(e.line, None);
        assert!(e.message.contains("no LUT_1D_SIZE"));
        let e = err("DOMAIN_MIN 1 0 0\nDOMAIN_MAX 0 1 1\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n");
        assert!(e.message.contains("domain minimum"), "{e}");
        assert!(parse_cube("").is_err());
        assert!(parse_cube("\u{0}\u{1}garbage").is_err());
    }

    #[test]
    fn tetrahedral_interpolation_matches_hand_computed_values() {
        // A 2-cube whose corner values are distinct: value(r,g,b) = (r + 2g + 4b) / 7 per channel,
        // blue channel scrambled so each channel is an independent affine function.
        let mut text = String::from("LUT_3D_SIZE 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    let (r, g, b) = (r as f32, g as f32, b as f32);
                    text.push_str(&format!("{} {} {}\n", r * r * 0.5 + g * 0.25, g * g * 0.75 + b * 0.125, b * 0.9 + r * g * 0.4));
                }
            }
        }
        let lut = parse_cube(&text).unwrap();
        // Corners: c000 = (0,0,0); c100 = (.5,0,0); c010 = (.25,.75,0); c110 = (.75,.75,.4);
        // c001 = (0,.125,.9); c101 = (.5,.125,.9); c011 = (.25,.875,.9); c111 = (.75,.875,1.3).
        // Point (0.6, 0.3, 0.1): fr > fg > fb -> c000 + fr*(c100-c000) + fg*(c110-c100) + fb*(c111-c110).
        let o = lut.sample([0.6, 0.3, 0.1]);
        let expect = [
            0.6 * 0.5 + 0.3 * (0.75 - 0.5) + 0.1 * (0.75 - 0.75),
            0.0 + 0.3 * (0.75 - 0.0) + 0.1 * (0.875 - 0.75),
            0.0 + 0.3 * (0.4 - 0.0) + 0.1 * (1.3 - 0.4),
        ];
        for c in 0..3 {
            assert!((o[c] - expect[c]).abs() < 1e-6, "{c}: {} vs {}", o[c], expect[c]);
        }
        // Point (0.2, 0.5, 0.9): fb > fg > fr -> c000 + fb*(c001-c000) + fg*(c011-c001) + fr*(c111-c011).
        let o = lut.sample([0.2, 0.5, 0.9]);
        let expect = [
            0.0 + 0.9 * 0.0 + 0.5 * (0.25 - 0.0) + 0.2 * (0.75 - 0.25),
            0.0 + 0.9 * 0.125 + 0.5 * (0.875 - 0.125) + 0.2 * (0.875 - 0.875),
            0.0 + 0.9 * 0.9 + 0.5 * (0.9 - 0.9) + 0.2 * (1.3 - 0.9),
        ];
        for c in 0..3 {
            assert!((o[c] - expect[c]).abs() < 1e-6, "{c}: {} vs {}", o[c], expect[c]);
        }
        // Point (0.5, 0.9, 0.2): fg > fr > fb -> c000 + fg*(c010-c000) + fr*(c110-c010) + fb*(c111-c110).
        let o = lut.sample([0.5, 0.9, 0.2]);
        let expect = [
            0.9 * 0.25 + 0.5 * (0.75 - 0.25) + 0.2 * (0.75 - 0.75),
            0.9 * 0.75 + 0.5 * (0.75 - 0.75) + 0.2 * (0.875 - 0.75),
            0.0 + 0.5 * (0.4 - 0.0) + 0.2 * (1.3 - 0.4),
        ];
        for c in 0..3 {
            assert!((o[c] - expect[c]).abs() < 1e-6, "{c}: {} vs {}", o[c], expect[c]);
        }
        // Corners are exact.
        assert_eq!(lut.sample([1.0, 1.0, 1.0]), [0.75, 0.875, 1.3]);
        assert_eq!(lut.sample([1.0, 0.0, 0.0]), [0.5, 0.0, 0.0]);
    }

    #[test]
    fn acescct_round_trips() {
        for x in [-0.005, 0.0, 0.002, 0.0078125, 0.01, 0.18, 1.0, 16.0, 200.0] {
            let y = acescct_decode(acescct_encode(x));
            assert!((y - x).abs() <= 1e-5 * x.abs().max(1.0), "{x} -> {y}");
        }
        assert!((acescct_encode(0.18) - 0.4135884).abs() < 1e-4);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rawgraph-lut-core-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cache_reuses_an_unchanged_file_and_reloads_a_changed_one() {
        let dir = temp_dir("cache");
        let path = dir.join("a.cube");
        std::fs::write(&path, identity_3d(3)).unwrap();
        let first = load_cached(&path).unwrap();
        let second = load_cached(&path).unwrap();
        assert!(Arc::ptr_eq(&first, &second), "an unchanged file is not parsed again");
        // A different size invalidates (the modification time may not move within a test).
        std::fs::write(&path, identity_3d(4)).unwrap();
        let third = load_cached(&path).unwrap();
        assert!(!Arc::ptr_eq(&first, &third));
        assert_eq!(third.lut3d.as_ref().unwrap().size, 4);
        // Deleting the file is an error, not a stale hit.
        std::fs::remove_file(&path).unwrap();
        let e = load_cached(&path).unwrap_err();
        assert!(e.message.contains("cannot read"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_broken_files_are_validate_issues_not_errors() {
        use crate::graph::{Graph, ImageId, Mode, NodeData, NodeKind};
        let dir = temp_dir("validate");
        let good = dir.join("good.cube");
        std::fs::write(&good, identity_3d(2)).unwrap();
        let bad = dir.join("bad.cube");
        std::fs::write(&bad, "LUT_3D_SIZE 2\n0 0 0\nzzz\n").unwrap();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let lut = g.add_node(NodeKind::Lut, [1.0, 0.0]);
        g.connect(src, lut, 0).unwrap();
        let issues = |g: &Graph| g.validate().into_iter().filter(|i| i.node == lut).map(|i| i.message).collect::<Vec<_>>();
        assert!(issues(&g).is_empty(), "no file chosen yet is not a problem");
        let set = |g: &mut Graph, img: u64, p: &Path| {
            assert!(g.node_mut(lut).unwrap().set_data(ImageId(img), Some(NodeData::LutFile(p.to_string_lossy().into_owned()))));
        };
        set(&mut g, 0, &good);
        assert!(issues(&g).is_empty());
        set(&mut g, 0, &dir.join("missing.cube"));
        let m = issues(&g);
        assert_eq!(m.len(), 1, "{m:?}");
        assert!(m[0].contains("missing.cube") && m[0].contains("cannot read"), "{m:?}");
        set(&mut g, 0, &bad);
        let m = issues(&g);
        assert!(m[0].contains("line 3"), "{m:?}");
        // Individual mode: a different file per image, each one checked.
        set(&mut g, 0, &good);
        g.node_mut(lut).unwrap().mode = Mode::Individual;
        set(&mut g, 7, &bad);
        let m = issues(&g);
        assert_eq!(m.len(), 1, "{m:?}");
        assert!(m[0].contains("image 7"), "{m:?}");
        let n = g.node(lut).unwrap();
        assert_eq!(crate::graph::lut_file(n, ImageId(7)), Some(bad.to_string_lossy().as_ref()));
        assert_eq!(crate::graph::lut_file(n, ImageId(8)), Some(good.to_string_lossy().as_ref()), "other images inherit the All file");
        // Clearing the path means no file.
        assert!(g.node_mut(lut).unwrap().set_data(ImageId(7), Some(NodeData::LutFile(String::new()))));
        assert!(issues(&g).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_file_is_an_error_with_its_line() {
        let dir = temp_dir("broken");
        let path = dir.join("b.cube");
        std::fs::write(&path, "LUT_3D_SIZE 2\n0 0 0\nnope\n").unwrap();
        let e = load_cached(&path).unwrap_err();
        assert_eq!(e.line, Some(3));
        assert!(load_cached(&dir).is_err(), "a folder is not a LUT");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
