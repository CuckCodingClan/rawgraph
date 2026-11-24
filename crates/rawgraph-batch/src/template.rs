//! File-name templates: `{name}`, `{index}`, `{index:04}`, `{id}`.

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Token {
    Literal(String),
    Name,
    /// Zero-padded to this width (0 = no padding).
    Index(usize),
    Id,
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
                    other => match other.strip_prefix("index:") {
                        Some(w) if !w.is_empty() && w.chars().all(|d| d.is_ascii_digit()) => {
                            let width: usize = w.parse().map_err(|_| format!("invalid width in {{{other}}}"))?;
                            if width > 32 {
                                return Err(format!("width in {{{other}}} is too large (max 32)"));
                            }
                            Token::Index(width)
                        }
                        _ => return Err(format!("unknown placeholder {{{other}}}")),
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

pub(crate) fn expand(tokens: &[Token], stem: &str, index: usize, id: u64) -> String {
    let mut out = String::new();
    for t in tokens {
        match t {
            Token::Literal(s) => out.push_str(s),
            Token::Name => out.push_str(stem),
            Token::Index(w) => out.push_str(&format!("{index:0w$}", w = *w)),
            Token::Id => out.push_str(&id.to_string()),
        }
    }
    out
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
        assert_eq!(expand(&t, "IMG", 7, 42), "IMG_0007-42");
        let t = parse("x{index}").unwrap_or_default();
        assert_eq!(expand(&t, "a", 12, 1), "x12");
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
