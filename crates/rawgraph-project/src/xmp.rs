//! XMP culling data (rating, colour label and keywords) and its import into a project.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use rawgraph_core::ImageId;

use crate::{ColorLabel, Flag, Project, ProjectError};

const XMP_NS: &str = "http://ns.adobe.com/xap/1.0/";
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const LR_NS: &str = "http://ns.adobe.com/lightroom/1.0/";
/// Real packets are a few KiB; anything beyond this is not a sidecar.
const MAX_XMP_BYTES: usize = 4 * 1024 * 1024;

/// Rating, colour label and keywords as found in an XMP packet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct XmpCulling {
    /// As written: -1 = rejected in Adobe tools, 0..=5.
    pub rating: Option<i8>,
    pub label: Option<String>,
    /// Keywords: the paths of `lr:hierarchicalSubject` (`Parent|Child`) plus the names of
    /// `dc:subject` that no path ends in, normalised and sorted. Empty when the packet has none.
    pub keywords: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Rating,
    Label,
}

/// The two keyword lists of a packet.
#[derive(Clone, Copy, PartialEq)]
enum KeywordList {
    /// `dc:subject`: flat names.
    Flat,
    /// `lr:hierarchicalSubject`: `Parent|Child` paths.
    Hierarchical,
}

#[derive(Default)]
struct Keywords {
    flat: Vec<String>,
    hierarchical: Vec<String>,
    list: Option<KeywordList>,
    in_item: bool,
    text: String,
}

impl Keywords {
    fn finish(self, out: &mut XmpCulling) {
        out.keywords = rawgraph_core::keywords::merge_xmp_keywords(&self.flat, &self.hierarchical);
    }
}

fn list_of(prefixes: &HashMap<String, String>, qname: &[u8]) -> Option<KeywordList> {
    let (prefix, local) = split_name(qname);
    let (prefix, local) = (std::str::from_utf8(prefix).ok()?, std::str::from_utf8(local).ok()?);
    let uri = prefixes.get(prefix).map(String::as_str);
    match local {
        "subject" if uri == Some(DC_NS) || (uri.is_none() && prefix == "dc") => Some(KeywordList::Flat),
        "hierarchicalSubject" if uri == Some(LR_NS) || (uri.is_none() && prefix == "lr") => Some(KeywordList::Hierarchical),
        _ => None,
    }
}

fn is_list_item(qname: &[u8]) -> bool {
    split_name(qname).1 == b"li"
}

fn split_name(name: &[u8]) -> (&[u8], &[u8]) {
    match name.iter().position(|&b| b == b':') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (&[][..], name),
    }
}

fn parse_rating(text: &str) -> Option<i8> {
    let t = text.trim();
    let v = t.parse::<i8>().ok().or_else(|| {
        let f = t.parse::<f32>().ok()?;
        if !f.is_finite() || f.abs() > 100.0 {
            return None;
        }
        i8::try_from(f.round() as i32).ok()
    })?;
    (-1..=5).contains(&v).then_some(v)
}

fn field_of(prefixes: &HashMap<String, String>, qname: &[u8]) -> Option<Field> {
    let (prefix, local) = split_name(qname);
    let local = std::str::from_utf8(local).ok()?;
    let prefix = std::str::from_utf8(prefix).ok()?;
    let in_xmp = match prefixes.get(prefix) {
        Some(uri) => uri == XMP_NS,
        None => prefix == "xmp",
    };
    if !in_xmp {
        return None;
    }
    match local {
        "Rating" => Some(Field::Rating),
        "Label" => Some(Field::Label),
        _ => None,
    }
}

fn store(out: &mut XmpCulling, field: Field, text: &str) {
    match field {
        Field::Rating => {
            if out.rating.is_none() {
                out.rating = parse_rating(text);
            }
        }
        Field::Label => {
            let t = text.trim();
            if out.label.is_none() && !t.is_empty() {
                out.label = Some(t.to_string());
            }
        }
    }
}

fn scan_attributes(prefixes: &mut HashMap<String, String>, out: &mut XmpCulling, e: &BytesStart<'_>) {
    let attrs: Vec<(Vec<u8>, String)> = e
        .attributes()
        .map_while(Result::ok)
        .map(|a| (a.key.as_ref().to_vec(), String::from_utf8_lossy(&a.value).into_owned()))
        .collect();
    for (key, value) in &attrs {
        if let Some(p) = key.strip_prefix(b"xmlns:") {
            prefixes.insert(String::from_utf8_lossy(p).into_owned(), value.clone());
        }
    }
    for (key, value) in &attrs {
        if let Some(field) = field_of(prefixes, key) {
            store(out, field, value);
        }
    }
}

/// Parses an XMP packet (sidecar content or an embedded packet) and extracts
/// `xmp:Rating` and `xmp:Label`, written as attributes or child elements, and the keywords of
/// `dc:subject` and `lr:hierarchicalSubject`.
/// Input that is not XMP yields `Ok(default)` or an error, never a panic.
pub fn parse_xmp_culling(xmp: &str) -> Result<XmpCulling, ProjectError> {
    if xmp.len() > MAX_XMP_BYTES {
        return Err(ProjectError::Corrupt("XMP data is too large".into()));
    }
    let mut reader = Reader::from_str(xmp);
    let mut prefixes: HashMap<String, String> = HashMap::new();
    let mut out = XmpCulling::default();
    let mut kw = Keywords::default();
    let mut current: Option<Field> = None;
    loop {
        let event = match reader.read_event() {
            Ok(ev) => ev,
            Err(e) => {
                if out.rating.is_some() || out.label.is_some() || !kw.flat.is_empty() || !kw.hierarchical.is_empty() {
                    kw.finish(&mut out);
                    return Ok(out);
                }
                return Err(ProjectError::Corrupt(format!("XMP is not well-formed: {e}")));
            }
        };
        match event {
            Event::Start(e) => {
                scan_attributes(&mut prefixes, &mut out, &e);
                current = field_of(&prefixes, e.name().as_ref());
                if let Some(list) = list_of(&prefixes, e.name().as_ref()) {
                    kw.list = Some(list);
                } else if kw.list.is_some() && is_list_item(e.name().as_ref()) {
                    kw.in_item = true;
                    kw.text.clear();
                }
            }
            Event::Empty(e) => {
                scan_attributes(&mut prefixes, &mut out, &e);
                current = None;
            }
            Event::Text(t) => {
                let text = String::from_utf8_lossy(t.as_ref()).into_owned();
                if kw.in_item {
                    kw.text.push_str(&text);
                }
                if let Some(field) = current {
                    store(&mut out, field, &text);
                }
            }
            Event::CData(t) if kw.in_item => kw.text.push_str(&String::from_utf8_lossy(t.as_ref())),
            Event::GeneralRef(r) if kw.in_item => match r.resolve_char_ref() {
                Ok(Some(c)) => kw.text.push(c),
                _ => match r.as_ref() {
                    b"amp" => kw.text.push('&'),
                    b"lt" => kw.text.push('<'),
                    b"gt" => kw.text.push('>'),
                    b"quot" => kw.text.push('"'),
                    b"apos" => kw.text.push('\''),
                    _ => {}
                },
            },
            Event::End(e) => {
                current = None;
                if kw.in_item && is_list_item(e.name().as_ref()) {
                    kw.in_item = false;
                    let text = std::mem::take(&mut kw.text);
                    match kw.list {
                        Some(KeywordList::Flat) => kw.flat.push(text),
                        Some(KeywordList::Hierarchical) => kw.hierarchical.push(text),
                        None => {}
                    }
                } else if list_of(&prefixes, e.name().as_ref()).is_some() {
                    kw.list = None;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    kw.finish(&mut out);
    Ok(out)
}

/// Path of the sidecar of `image` (same folder and stem, extension `.xmp` in
/// any letter case). `Some` only if the file exists.
pub fn xmp_sidecar_path(image: &Path) -> Option<PathBuf> {
    for ext in ["xmp", "XMP"] {
        let p = image.with_extension(ext);
        if p.is_file() {
            return Some(p);
        }
    }
    let stem = image.file_stem()?.to_os_string();
    let parent = image.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    for entry in std::fs::read_dir(parent).ok()?.flatten() {
        let p = entry.path();
        let ext_ok = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("xmp"));
        if ext_ok && p.file_stem() == Some(stem.as_os_str()) && p.is_file() {
            return Some(p);
        }
    }
    None
}

fn parse_label(text: &str) -> Option<ColorLabel> {
    match text.trim().to_lowercase().as_str() {
        "red" | "rot" => Some(ColorLabel::Red),
        "yellow" | "gelb" => Some(ColorLabel::Yellow),
        "green" | "grün" | "gruen" => Some(ColorLabel::Green),
        "blue" | "blau" => Some(ColorLabel::Blue),
        "purple" | "lila" => Some(ColorLabel::Purple),
        _ => None,
    }
}

fn read_sidecar(path: &Path) -> Option<XmpCulling> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_XMP_BYTES as u64 + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > MAX_XMP_BYTES {
        return None;
    }
    parse_xmp_culling(&String::from_utf8_lossy(&bytes)).ok()
}

impl Project {
    /// Applies XMP culling data to an entry: rating 1..=5 sets the rating, -1
    /// sets [`Flag::Reject`] (rating unchanged), 0 or `None` change nothing; a
    /// known label name (English or German Lightroom names) sets the colour
    /// label. Returns whether anything changed.
    pub fn apply_xmp_culling(&mut self, id: ImageId, xmp: &XmpCulling) -> bool {
        let Some(entry) = self.image_mut(id) else { return false };
        let mut changed = false;
        match xmp.rating {
            Some(r @ 1..=5) => {
                let r = r as u8;
                if entry.rating != r {
                    entry.rating = r;
                    changed = true;
                }
            }
            Some(-1) if entry.flag != Flag::Reject => {
                entry.flag = Flag::Reject;
                changed = true;
            }
            _ => {}
        }
        if let Some(label) = xmp.label.as_deref().and_then(parse_label) {
            if entry.color_label != Some(label) {
                entry.color_label = Some(label);
                changed = true;
            }
        }
        changed
    }

    /// Adds the keywords of an XMP packet to an entry (a union: nothing is removed). Returns whether
    /// anything changed.
    pub fn apply_xmp_keywords(&mut self, id: ImageId, xmp: &XmpCulling) -> bool {
        if xmp.keywords.is_empty() {
            return false;
        }
        let Some(entry) = self.image_mut(id) else { return false };
        crate::keywords::add_to_list(&mut entry.keywords, &xmp.keywords)
    }

    /// For every non-missing entry with a sidecar: read, parse, apply. With
    /// `only_unrated`, only entries that still have rating 0, no flag and no
    /// label are touched. Unreadable or broken sidecars are skipped. The keywords of the
    /// sidecars are merged into the entries (a union, nothing is removed). Returns the ids that changed.
    pub fn import_xmp_sidecars(&mut self, only_unrated: bool) -> Vec<ImageId> {
        let todo: Vec<(ImageId, PathBuf)> = self
            .images()
            .iter()
            .filter(|e| !e.missing)
            .filter(|e| !only_unrated || (e.rating == 0 && e.flag == Flag::None && e.color_label.is_none()))
            .filter_map(|e| xmp_sidecar_path(&e.path).map(|p| (e.id, p)))
            .collect();
        let mut changed = Vec::new();
        for (id, sidecar) in todo {
            if let Some(xmp) = read_sidecar(&sidecar) {
                let a = self.apply_xmp_culling(id, &xmp);
                let b = self.apply_xmp_keywords(id, &xmp);
                if a || b {
                    changed.push(id);
                }
            }
        }
        changed
    }
}
