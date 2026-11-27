//! Content hashing, natural ordering and path keys.

use std::cmp::Ordering;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// Size of the hashed head and tail windows.
pub const HASH_WINDOW: u64 = 64 * 1024;

#[cfg(test)]
/// Every path passed to `content_hash` (test hook).
pub static HASHED: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Partial content hash of a file, returned together with the file size.
///
/// The hash is BLAKE3 over the file size (8 little-endian bytes), the first
/// 64 KiB and the last 64 KiB of the file, as a lowercase hex string. When the
/// file is smaller than 128 KiB the two ranges never overlap: the tail starts
/// where the head ends, so every byte is hashed at most once.
///
/// This is deliberately a *partial* hash: it reads at most 128 KiB per file, so
/// scanning a folder stays fast on network storage. The price is that two
/// files that agree in size, first and last 64 KiB but differ in the middle
/// get the same hash. For camera files this is practically irrelevant, and the
/// hash is only used to recognise a moved or renamed file, never to trust it.
pub fn content_hash(path: &Path) -> io::Result<(String, u64)> {
    #[cfg(test)]
    if let Ok(mut v) = HASHED.lock() {
        v.push(path.to_path_buf());
    }
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());

    let head_len = size.min(HASH_WINDOW);
    let mut buf = vec![0u8; usize::try_from(head_len).unwrap_or(0)];
    file.read_exact(&mut buf)?;
    hasher.update(&buf);

    let tail_start = size.saturating_sub(HASH_WINDOW).max(head_len);
    let tail_len = size - tail_start;
    if tail_len > 0 {
        file.seek(SeekFrom::Start(tail_start))?;
        buf.resize(usize::try_from(tail_len).unwrap_or(0), 0);
        file.read_exact(&mut buf)?;
        hasher.update(&buf);
    }
    Ok((hasher.finalize().to_hex().to_string(), size))
}

/// Case-insensitive natural comparison: digit runs compare by numeric value,
/// so "img2" sorts before "img10". Equal-looking strings fall back to the
/// plain comparison so the order is total and deterministic.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.char_indices().peekable(), b.char_indices().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some((_, x)), Some((_, y))) => {
                if x.is_ascii_digit() && y.is_ascii_digit() {
                    let da = take_digits(a, &mut ai);
                    let db = take_digits(b, &mut bi);
                    let (ta, tb) = (da.trim_start_matches('0'), db.trim_start_matches('0'));
                    let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    let ord = if x == y { Ordering::Equal } else { lower(x).cmp(&lower(y)) };
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
}

fn lower(c: char) -> char {
    if c.is_ascii() { c.to_ascii_lowercase() } else { c.to_lowercase().next().unwrap_or(c) }
}

/// The run of ASCII digits at the iterator's position, as a slice of `s` (no allocation:
/// scan sorting compares every path pair this way).
fn take_digits<'a>(s: &'a str, it: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> &'a str {
    let start = it.peek().map_or(s.len(), |&(i, _)| i);
    let mut end = start;
    while let Some(&(i, c)) = it.peek() {
        if !c.is_ascii_digit() {
            break;
        }
        end = i + 1;
        it.next();
    }
    &s[start..end]
}

/// Natural order of two paths, component by component.
pub fn path_cmp(a: &Path, b: &Path) -> Ordering {
    let (mut ac, mut bc) = (a.components(), b.components());
    loop {
        match (ac.next(), bc.next()) {
            (Some(x), Some(y)) => {
                let ord = natural_cmp(&x.as_os_str().to_string_lossy(), &y.as_os_str().to_string_lossy());
                if ord != Ordering::Equal { return ord; }
            }
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (None, None) => return a.cmp(b),
        }
    }
}

/// Sorts listed files below `folder` in [`path_cmp`] order. Every path shares the
/// folder prefix, so only the part below it is compared.
pub fn sort_under<T>(folder: &Path, files: &mut [(std::path::PathBuf, T)]) {
    if files.iter().all(|(p, _)| p.starts_with(folder)) {
        // Components are converted once per file rather than once per comparison.
        files.sort_by_cached_key(|(p, _)| NaturalPath::new(p.strip_prefix(folder).unwrap_or(p), p));
    } else {
        files.sort_by(|a, b| path_cmp(&a.0, &b.0));
    }
}

/// Sort key equal to [`path_cmp`] order for paths below one folder.
#[derive(PartialEq, Eq)]
struct NaturalPath(Vec<String>, std::path::PathBuf);

impl NaturalPath {
    fn new(rel: &Path, full: &Path) -> Self {
        Self(rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect(), full.to_path_buf())
    }
}

impl Ord for NaturalPath {
    fn cmp(&self, other: &Self) -> Ordering {
        for (x, y) in self.0.iter().zip(&other.0) {
            let ord = natural_cmp(x, y);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        self.0.len().cmp(&other.0.len()).then_with(|| self.1.cmp(&other.1))
    }
}

impl PartialOrd for NaturalPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Key under which paths are compared for identity (case-insensitive on Windows).
pub fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}
