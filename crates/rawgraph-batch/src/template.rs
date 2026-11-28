//! File-name templates: `{name}`, `{index}`, `{index:04}`, `{id}`, and the caption-sheet tokens
//! `{sheet_name}` (sheet column B), `{caption}` (column C) and `{no}` / `{no:03}` (photo number).

use rawgraph_rename::SheetBinding;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Token {
    Literal(String),
    Name,
    /// Zero-padded to this width (0 = no padding).
    Index(usize),
    Id,
    SheetName,
    Caption,
    /// Photo number found in the file name, zero-padded to this width.
    No(usize),
}

const INVALID_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
    "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn bad_char(s: &str) -> Option<char> {
    s.chars().find(|c| INVALID_CHARS.contains(c) || (*c as u32) < 32)
}

pub(crate) fn parse(template: &str) -> Result<Vec<Token>, String> {
    if template.trim().is_empty() {
        return Err("the name template is empty".into());
    }
    let mut tokens = Vec::new();
    let mut lit = String::new();
    let mut chars = template.chars();
    let flush = |lit: &mut String, tokens: &mut Vec<Token>| -> Result<(), String> {
        if lit.is_empty() {
            return Ok(());
        }
        if let Some(c) = bad_char(lit) {
            return Err(format!("the name template contains the character {c:?}, which is not allowed in file names"));
        }
        tokens.push(Token::Literal(std::mem::take(lit)));
        Ok(())
    };
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                flush(&mut lit, &mut tokens)?;
                let mut body = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some('{') | None => return Err("the name template has unbalanced braces".into()),
                        Some(c) => body.push(c),
                    }
                }
                tokens.push(match body.as_str() {
                    "name" => Token::Name,
                    "id" => Token::Id,
                    "index" => Token::Index(0),
                    "sheet_name" => Token::SheetName,
                    "caption" => Token::Caption,
                    "no" => Token::No(0),
                    other => match other.strip_prefix("no:") {
                        Some(w) if !w.is_empty() && w.chars().all(|d| d.is_ascii_digit()) => {
                            let width: usize = w.parse().map_err(|_| format!("invalid width in {{{other}}}"))?;
                            if width > 32 {
                                return Err(format!("width in {{{other}}} is too large (max 32)"));
                            }
                            Token::No(width)
                        }
                        _ => match other.strip_prefix("index:") {
                        Some(w) if !w.is_empty() && w.chars().all(|d| d.is_ascii_digit()) => {
                            let width: usize = w.parse().map_err(|_| format!("invalid width in {{{other}}}"))?;
                            if width > 32 {
                                return Err(format!("width in {{{other}}} is too large (max 32)"));
                            }
                            Token::Index(width)
                        }
                        _ => return Err(format!("unknown placeholder {{{other}}}")),
                        },
                    },
                });
            }
            '}' => return Err("the name template has unbalanced braces".into()),
            c => lit.push(c),
        }
    }
    flush(&mut lit, &mut tokens)?;
    Ok(tokens)
}

/// Whether the template needs a caption sheet (`{sheet_name}` or `{caption}`).
pub(crate) fn needs_sheet(tokens: &[Token]) -> bool {
    tokens.iter().any(|t| matches!(t, Token::SheetName | Token::Caption))
}

/// Fills the template. The sheet tokens fail with a plain sentence when the image has no photo number
/// or no row in the sheet: the caller reports that for this image only.
pub(crate) fn expand(tokens: &[Token], stem: &str, index: usize, id: u64, sheet: Option<&SheetBinding>) -> Result<String, String> {
    let mut out = String::new();
    for t in tokens {
        match t {
            Token::Literal(s) => out.push_str(s),
            Token::Name => out.push_str(stem),
            Token::Index(w) => out.push_str(&format!("{index:0w$}", w = *w)),
            Token::Id => out.push_str(&id.to_string()),
            Token::SheetName | Token::Caption => {
                let b = sheet.ok_or("the name template needs a caption sheet, but none is loaded")?;
                let r = b.resolve(stem)?;
                out.push_str(if *t == Token::SheetName { &r.name } else { &r.caption });
            }
            Token::No(w) => {
                let pattern = sheet.map(|b| b.pattern.clone()).unwrap_or_default();
                let n = pattern.extract(stem).ok_or_else(|| format!("no photo number found in the file name {stem:?}"))?;
                out.push_str(&format!("{n:0w$}", w = *w));
            }
        }
    }
    Ok(out)
}

/// Checks an expanded file-name stem (without extension).
pub(crate) fn check_stem(name: &str) -> Result<(), String> {
    if name.trim_matches(|c| c == '.' || c == ' ').is_empty() {
        return Err(format!("the expanded file name {name:?} is empty"));
    }
    if let Some(c) = bad_char(name) {
        return Err(format!("the expanded file name {name:?} contains the character {c:?}, which is not allowed"));
    }
    let head = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    if RESERVED.contains(&head.as_str()) {
        return Err(format!("the expanded file name {name:?} is a reserved Windows device name"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_expands() {
        let t = parse("{name}_{index:04}-{id}").unwrap_or_default();
        assert_eq!(expand(&t, "IMG", 7, 42, None).unwrap(), "IMG_0007-42");
        let t = parse("x{index}").unwrap_or_default();
        assert_eq!(expand(&t, "a", 12, 1, None).unwrap(), "x12");
    }

    #[test]
    fn sheet_tokens_resolve_through_the_binding() {
        use rawgraph_rename::{CaptionSheet, NumberPattern, SheetBinding, SheetColumns};
        let cells = vec![vec!["76".to_owned(), "076_Anna".to_owned(), "Anna Berger".to_owned()]];
        let b = SheetBinding { sheet: CaptionSheet::from_cells(&cells, &SheetColumns::default()).unwrap(), pattern: NumberPattern::default() };
        let t = parse("{sheet_name}_{caption}_{no:04}_{no}").unwrap();
        assert!(needs_sheet(&t));
        assert_eq!(expand(&t, "Job-0076", 1, 1, Some(&b)).unwrap(), "076_Anna_Anna Berger_0076_76");
        // No row, no number, no sheet: a plain error per image, never a fallback name.
        assert!(expand(&t, "Job-0077", 1, 1, Some(&b)).unwrap_err().contains("no row"));
        assert!(expand(&t, "Job", 1, 1, Some(&b)).unwrap_err().contains("no photo number"));
        assert!(expand(&t, "Job-0076", 1, 1, None).unwrap_err().contains("caption sheet"));
        // {no} alone needs no sheet.
        assert_eq!(expand(&parse("x{no:03}").unwrap(), "IMG_5", 1, 1, None).unwrap(), "x005");
        assert!(!needs_sheet(&parse("{no}").unwrap()));
        assert!(parse("{no:}").is_err() && parse("{no:99}").is_err());
    }

    #[test]
    fn rejects_bad_templates() {
        for bad in ["", "  ", "{nam}", "{name", "name}", "{{name}}", "a/{name}", "{name}:x", "{index:}", "{index:x}", "a*b"] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(check_stem("CON").is_err());
        assert!(check_stem("con.txt").is_err());
        assert!(check_stem("..").is_err());
        assert!(check_stem("IMG_1").is_ok());
    }
}
