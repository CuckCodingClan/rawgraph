//! The naming rules of the batch rename: a template with tokens, then find/replace, then a case
//! change, plus the extension case.
//!
//! Tokens: `{stem}` (original name without extension, also `{name}`), `{counter}` / `{counter:4}`
//! (start, step and padding come from [`Counter`]), `{date}` (20261128), `{time}` (142530),
//! `{datetime}` (20261128_142530), `{camera}`, `{sheet_name}` (sheet column B), `{caption}`
//! (column C), `{no}` / `{no:03}` (the photo number found in the name).

use regex::{NoExpand, Regex, RegexBuilder};

use crate::numbers::NumberPattern;
use crate::sheet::Resolved;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counter {
    pub start: i64,
    pub step: i64,
    /// Zero-padding width used by a plain `{counter}`.
    pub pad: usize,
}

impl Default for Counter {
    fn default() -> Self {
        Counter { start: 1, step: 1, pad: 3 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaseChange {
    #[default]
    None,
    Lower,
    Upper,
    /// First letter of every word upper case, the rest lower case.
    Title,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExtCase {
    #[default]
    Keep,
    Lower,
    Upper,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameRules {
    pub template: String,
    pub counter: Counter,
    pub find: String,
    pub replace: String,
    /// `find` is a regular expression (`${1}` in `replace` refers to groups).
    pub regex: bool,
    pub case_sensitive: bool,
    pub case: CaseChange,
    pub ext_case: ExtCase,
    pub number_pattern: NumberPattern,
}

impl Default for RenameRules {
    fn default() -> Self {
        RenameRules {
            template: "{stem}".into(),
            counter: Counter::default(),
            find: String::new(),
            replace: String::new(),
            regex: false,
            case_sensitive: true,
            case: CaseChange::None,
            ext_case: ExtCase::Keep,
            number_pattern: NumberPattern::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    Lit(String),
    Stem,
    Counter(Option<usize>),
    Date,
    Time,
    DateTime,
    Camera,
    SheetName,
    Caption,
    No(usize),
}

/// What a template needs to know about one file.
pub struct FileFacts<'a> {
    pub stem: &'a str,
    /// Position in the batch, 0-based.
    pub index: usize,
    /// EXIF camera-local time (ms on the Unix timeline), if known.
    pub capture_unix_ms: Option<i64>,
    pub camera: Option<&'a str>,
    /// Photo number extracted from the name.
    pub number: Option<u64>,
    /// The sheet row of that number (when a sheet is used).
    pub sheet: Option<&'a Resolved>,
}

/// Rules ready to run (template parsed, expressions compiled).
#[derive(Clone, Debug)]
pub struct CompiledRules {
    toks: Vec<Tok>,
    counter: Counter,
    find: Option<(Regex, String, bool)>,
    case: CaseChange,
    ext_case: ExtCase,
    pub number_pattern: NumberPattern,
    pub uses_sheet: bool,
    pub uses_caption: bool,
    pub uses_number: bool,
    pub uses_capture: bool,
}

impl RenameRules {
    pub fn compile(&self) -> Result<CompiledRules, String> {
        let toks = parse_template(&self.template)?;
        let find = if self.find.is_empty() {
            None
        } else {
            let pat = if self.regex { self.find.clone() } else { regex::escape(&self.find) };
            let re = RegexBuilder::new(&pat).case_insensitive(!self.case_sensitive).build().map_err(|e| format!("the Find expression is not valid: {e}"))?;
            Some((re, self.replace.clone(), self.regex))
        };
        // A zero step is allowed; it gives equal names, which the duplicate check reports.
        let uses_sheet = toks.iter().any(|t| matches!(t, Tok::SheetName | Tok::Caption));
        let uses_caption = toks.iter().any(|t| matches!(t, Tok::Caption));
        let uses_number = toks.iter().any(|t| matches!(t, Tok::No(_)));
        let uses_capture = toks.iter().any(|t| matches!(t, Tok::Date | Tok::Time | Tok::DateTime | Tok::Camera));
        Ok(CompiledRules { toks, counter: self.counter, find, case: self.case, ext_case: self.ext_case, number_pattern: self.number_pattern.clone(), uses_sheet, uses_caption, uses_number, uses_capture })
    }
}

impl CompiledRules {
    /// The new name stem (find/replace and case change applied).
    pub fn new_stem(&self, f: &FileFacts) -> Result<String, String> {
        let mut out = String::new();
        for t in &self.toks {
            match t {
                Tok::Lit(s) => out.push_str(s),
                Tok::Stem => out.push_str(f.stem),
                Tok::Counter(pad) => {
                    let v = self.counter.start.checked_add(self.counter.step.checked_mul(f.index as i64).ok_or("the counter overflows")?).ok_or("the counter overflows")?;
                    if v < 0 {
                        return Err(format!("the counter would be negative ({v}) at item {}", f.index + 1));
                    }
                    let w = pad.unwrap_or(self.counter.pad);
                    out.push_str(&format!("{v:0w$}"));
                }
                Tok::Date | Tok::Time | Tok::DateTime => {
                    let ms = f.capture_unix_ms.ok_or("the file has no capture date")?;
                    let (y, mo, d, h, mi, s) = civil(ms);
                    match t {
                        Tok::Date => out.push_str(&format!("{y:04}{mo:02}{d:02}")),
                        Tok::Time => out.push_str(&format!("{h:02}{mi:02}{s:02}")),
                        _ => out.push_str(&format!("{y:04}{mo:02}{d:02}_{h:02}{mi:02}{s:02}")),
                    }
                }
                Tok::Camera => {
                    let c = f.camera.filter(|c| !c.trim().is_empty()).ok_or("the file has no camera model")?;
                    out.push_str(&clean_value(c));
                }
                Tok::SheetName => {
                    let r = f.sheet.ok_or("no caption sheet row for this file")?;
                    out.push_str(&r.name);
                }
                Tok::Caption => {
                    let r = f.sheet.ok_or("no caption sheet row for this file")?;
                    out.push_str(&r.caption);
                }
                Tok::No(w) => {
                    let n = f.number.ok_or_else(|| format!("no photo number found in {:?}", f.stem))?;
                    out.push_str(&format!("{n:0w$}", w = *w));
                }
            }
        }
        if let Some((re, rep, is_regex)) = &self.find {
            out = if *is_regex { re.replace_all(&out, rep.as_str()).into_owned() } else { re.replace_all(&out, NoExpand(rep.as_str())).into_owned() };
        }
        Ok(match self.case {
            CaseChange::None => out,
            CaseChange::Lower => out.to_lowercase(),
            CaseChange::Upper => out.to_uppercase(),
            CaseChange::Title => title_case(&out),
        })
    }

    /// `ext` (without the dot) in the chosen case.
    pub fn new_ext(&self, ext: &str) -> String {
        match self.ext_case {
            ExtCase::Keep => ext.to_owned(),
            ExtCase::Lower => ext.to_lowercase(),
            ExtCase::Upper => ext.to_uppercase(),
        }
    }

    pub fn ext_case(&self) -> ExtCase {
        self.ext_case
    }
}

fn parse_template(t: &str) -> Result<Vec<Tok>, String> {
    if t.trim().is_empty() {
        return Err("the name template is empty".into());
    }
    let mut toks = Vec::new();
    let mut lit = String::new();
    let mut chars = t.chars();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                if !lit.is_empty() {
                    toks.push(Tok::Lit(std::mem::take(&mut lit)));
                }
                let mut body = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some('{') | None => return Err("the name template has unbalanced braces".into()),
                        Some(c) => body.push(c),
                    }
                }
                let (name, arg) = match body.split_once(':') {
                    Some((n, a)) => (n, Some(a)),
                    None => (body.as_str(), None),
                };
                let width = |a: Option<&str>| -> Result<Option<usize>, String> {
                    match a {
                        None => Ok(None),
                        Some(w) if !w.is_empty() && w.chars().all(|d| d.is_ascii_digit()) => {
                            let n: usize = w.parse().map_err(|_| format!("invalid width in {{{body}}}"))?;
                            if n > 32 { Err(format!("width in {{{body}}} is too large (max 32)")) } else { Ok(Some(n)) }
                        }
                        Some(_) => Err(format!("invalid width in {{{body}}}")),
                    }
                };
                toks.push(match name {
                    "stem" | "name" if arg.is_none() => Tok::Stem,
                    "counter" => Tok::Counter(width(arg)?),
                    "no" => Tok::No(width(arg)?.unwrap_or(0)),
                    "date" if arg.is_none() => Tok::Date,
                    "time" if arg.is_none() => Tok::Time,
                    "datetime" if arg.is_none() => Tok::DateTime,
                    "camera" if arg.is_none() => Tok::Camera,
                    "sheet_name" if arg.is_none() => Tok::SheetName,
                    "caption" if arg.is_none() => Tok::Caption,
                    _ => return Err(format!("unknown placeholder {{{body}}}")),
                });
            }
            '}' => return Err("the name template has unbalanced braces".into()),
            c => lit.push(c),
        }
    }
    if !lit.is_empty() {
        toks.push(Tok::Lit(lit));
    }
    Ok(toks)
}

/// Metadata values go into file names: characters a file name cannot hold become `-`, runs of
/// white space `_`.
fn clean_value(s: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in s.trim().chars() {
        if c.is_whitespace() {
            gap = true;
            continue;
        }
        if gap {
            out.push('_');
            gap = false;
        }
        out.push(if crate::names::is_bad_char(c) { '-' } else { c });
    }
    out
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut start = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if start {
                out.extend(c.to_uppercase());
            } else {
                out.extend(c.to_lowercase());
            }
            start = false;
        } else {
            out.push(c);
            start = true;
        }
    }
    out
}

/// (year, month, day, hour, minute, second) of a time given in ms since 1970-01-01 (proleptic Gregorian).
pub fn civil(ms: i64) -> (i64, u32, u32, u32, u32, u32) {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(stem: &'a str, index: usize) -> FileFacts<'a> {
        FileFacts { stem, index, capture_unix_ms: None, camera: None, number: None, sheet: None }
    }

    fn stem_of(rules: &RenameRules, f: &FileFacts) -> Result<String, String> {
        rules.compile()?.new_stem(f)
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
        // 2026-11-28 14:25:30 UTC
        assert_eq!(civil(1_795_875_930_000), (2026, 11, 28, 14, 25, 30));
        assert_eq!(civil(-1000), (1969, 12, 31, 23, 59, 59));
        assert_eq!(civil(951_782_400_000).0, 2000);
        assert_eq!(civil(951_782_400_000).1, 2, "2000-02-29 exists");
        assert_eq!(civil(951_782_400_000).2, 29);
    }

    #[test]
    fn counter_start_step_and_padding() {
        let mut r = RenameRules { template: "{counter}_{stem}".into(), counter: Counter { start: 10, step: 5, pad: 4 }, ..Default::default() };
        assert_eq!(stem_of(&r, &facts("a", 0)).unwrap(), "0010_a");
        assert_eq!(stem_of(&r, &facts("a", 3)).unwrap(), "0025_a");
        r.template = "{counter:2}".into();
        assert_eq!(stem_of(&r, &facts("a", 1)).unwrap(), "15");
        r.counter = Counter { start: 3, step: -2, pad: 1 };
        assert!(stem_of(&r, &facts("a", 5)).unwrap_err().contains("negative"));
    }

    #[test]
    fn date_time_camera_and_number_tokens() {
        let r = RenameRules { template: "{date}_{time}_{camera}_{no:03}".into(), ..Default::default() };
        let mut f = facts("x", 0);
        assert!(stem_of(&r, &f).unwrap_err().contains("capture date"));
        f.capture_unix_ms = Some(1_795_875_930_000);
        assert!(stem_of(&r, &f).unwrap_err().contains("camera"));
        f.camera = Some("Canon EOS R5/2");
        assert!(stem_of(&r, &f).unwrap_err().contains("photo number"));
        f.number = Some(7);
        assert_eq!(stem_of(&r, &f).unwrap(), "20261128_142530_Canon_EOS_R5-2_007");
    }

    #[test]
    fn sheet_tokens_need_a_row() {
        let r = RenameRules { template: "{sheet_name}".into(), ..Default::default() };
        let c = r.compile().unwrap();
        assert!(c.uses_sheet);
        assert!(c.new_stem(&facts("a", 0)).is_err());
        let row = Resolved { number: 76, name: "076_Anna".into(), caption: "Anna".into() };
        let mut f = facts("a", 0);
        f.sheet = Some(&row);
        assert_eq!(c.new_stem(&f).unwrap(), "076_Anna");
        let r2 = RenameRules { template: "{no:03}_{caption}".into(), ..Default::default() };
        f.number = Some(76);
        assert_eq!(r2.compile().unwrap().new_stem(&f).unwrap(), "076_Anna");
    }

    #[test]
    fn find_replace_plain_regex_and_case() {
        let mut r = RenameRules { template: "{stem}".into(), find: "a.b".into(), replace: "$1".into(), ..Default::default() };
        assert_eq!(stem_of(&r, &facts("xa.by a.b", 0)).unwrap(), "x$1y $1", "plain text and replacement are literal");
        r.case_sensitive = false;
        assert_eq!(stem_of(&r, &facts("A.B", 0)).unwrap(), "$1");
        r.regex = true;
        r.find = r"(\d+)-(\d+)".into();
        r.replace = "${2}_${1}".into();
        assert_eq!(stem_of(&r, &facts("12-34", 0)).unwrap(), "34_12");
        r.find = "(".into();
        assert!(r.compile().is_err());
        let mut c = RenameRules { case: CaseChange::Upper, ..Default::default() };
        assert_eq!(stem_of(&c, &facts("abc ä", 0)).unwrap(), "ABC Ä");
        c.case = CaseChange::Lower;
        assert_eq!(stem_of(&c, &facts("ABC", 0)).unwrap(), "abc");
        c.case = CaseChange::Title;
        assert_eq!(stem_of(&c, &facts("anna BERGER_x", 0)).unwrap(), "Anna Berger_X");
    }

    #[test]
    fn extension_case() {
        let mut r = RenameRules::default();
        assert_eq!(r.compile().unwrap().new_ext("JpG"), "JpG");
        r.ext_case = ExtCase::Lower;
        assert_eq!(r.compile().unwrap().new_ext("JPG"), "jpg");
        r.ext_case = ExtCase::Upper;
        assert_eq!(r.compile().unwrap().new_ext("cr2"), "CR2");
    }

    #[test]
    fn bad_templates() {
        for bad in ["", "  ", "{nam}", "{stem", "stem}", "{{stem}}", "{counter:}", "{counter:x}", "{date:3}", "{no:99}"] {
            assert!(RenameRules { template: bad.into(), ..Default::default() }.compile().is_err(), "{bad:?}");
        }
    }
}
