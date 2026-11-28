//! File-name rules shared by the planner: what a name may contain on the strictest platform
//! (Windows), and the key under which names are compared.

use std::path::Path;

pub const INVALID_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "COM0", "COM\u{b9}", "COM\u{b2}", "COM\u{b3}", "LPT0", "LPT\u{b9}", "LPT\u{b2}", "LPT\u{b3}", "CONIN$", "CONOUT$", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
    "LPT9",
];
/// The longest file name (in UTF-16 units on Windows, bytes elsewhere; the larger of both is used).
pub const MAX_NAME: usize = 255;

/// Why a file name cannot be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameProblem {
    Empty,
    InvalidChar(char),
    Reserved(String),
    TrailingDotOrSpace,
    TooLong(usize),
}

impl NameProblem {
    pub fn message(&self) -> String {
        match self {
            NameProblem::Empty => "the new name is empty".into(),
            NameProblem::InvalidChar(c) => format!("the new name contains {c:?}, which is not allowed in file names"),
            NameProblem::Reserved(n) => format!("{n} is a reserved Windows device name"),
            NameProblem::TrailingDotOrSpace => "the new name ends with a dot or a space".into(),
            NameProblem::TooLong(n) => format!("the new name is {n} characters long (the limit is {MAX_NAME})"),
        }
    }
}

/// Checks the complete new file name (`stem.ext`) and its stem. Returns every problem found.
pub fn check_name(stem: &str, name: &str) -> Vec<NameProblem> {
    let mut out = Vec::new();
    if stem.trim_matches(|c| c == '.' || c == ' ').is_empty() {
        out.push(NameProblem::Empty);
    }
    if let Some(c) = name.chars().find(|c| is_bad_char(*c)) {
        out.push(NameProblem::InvalidChar(c));
    }
    let head = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    if RESERVED.contains(&head.as_str()) {
        out.push(NameProblem::Reserved(head));
    }
    if name.ends_with('.') || name.ends_with(' ') || stem.ends_with('.') || stem.ends_with(' ') {
        out.push(NameProblem::TrailingDotOrSpace);
    }
    let len = name.encode_utf16().count().max(name.len());
    if len > MAX_NAME {
        out.push(NameProblem::TooLong(len));
    }
    out
}

/// Characters no name may contain: the Windows set, control characters (also C1) and the bidirectional
/// controls that make a name read differently from how it is spelled.
pub fn is_bad_char(c: char) -> bool {
    INVALID_CHARS.contains(&c) || is_control_or_bidi(c)
}

/// Control characters (also C1) and bidirectional controls: never allowed in the names of a sheet.
pub fn is_control_or_bidi(c: char) -> bool {
    let u = c as u32;
    u < 32 || (0x7f..=0x9f).contains(&u) || (0x202a..=0x202e).contains(&u) || (0x2066..=0x2069).contains(&u)
}

/// Whether file names differing only in case are the same file on this platform.
pub const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// Identity of a path for comparisons (lower case on case-insensitive platforms).
pub fn key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if CASE_INSENSITIVE {
        s.to_lowercase()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn good_and_bad_names() {
        assert!(check_name("076_Anna", "076_Anna.jpg").is_empty());
        assert!(check_name("a b", "a b.jpg").is_empty());
        assert!(check_name("", ".jpg").contains(&NameProblem::Empty));
        assert!(check_name("..", "...jpg").contains(&NameProblem::Empty));
        assert!(check_name("a/b", "a/b.jpg").contains(&NameProblem::InvalidChar('/')));
        assert!(check_name("a?", "a?.jpg").contains(&NameProblem::InvalidChar('?')));
        assert!(check_name("a\u{7}", "a\u{7}.jpg").iter().any(|p| matches!(p, NameProblem::InvalidChar(_))));
        for n in ["COM0", "lpt0", "CONIN$", "conout$", "COM\u{b9}", "LPT\u{b2}.txt"] {
            assert!(check_name(n, &format!("{n}.jpg")).iter().any(|p| matches!(p, NameProblem::Reserved(_))), "{n}");
        }
        for c in ['\u{85}', '\u{7f}', '\u{202e}', '\u{2066}'] {
            let n = format!("a{c}b");
            assert!(check_name(&n, &format!("{n}.jpg")).iter().any(|p| matches!(p, NameProblem::InvalidChar(_))), "{c:?}");
        }
        assert!(check_name("con", "con.jpg").contains(&NameProblem::Reserved("CON".into())));
        assert!(check_name("Lpt1", "Lpt1.txt").iter().any(|p| matches!(p, NameProblem::Reserved(_))));
        assert!(check_name("a.", "a..jpg").contains(&NameProblem::TrailingDotOrSpace));
        assert!(check_name("a ", "a .jpg").contains(&NameProblem::TrailingDotOrSpace));
        assert!(check_name("a", "a.").contains(&NameProblem::TrailingDotOrSpace));
        let long = "x".repeat(252);
        assert!(check_name(&long, &format!("{long}.jpg")).iter().any(|p| matches!(p, NameProblem::TooLong(256))));
        let ok = "x".repeat(251);
        assert!(check_name(&ok, &format!("{ok}.jpg")).is_empty(), "255 is still fine");
    }
}
