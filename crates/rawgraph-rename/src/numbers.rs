//! Photo numbers: reading the number out of a file name.

use regex::Regex;

/// A compiled regular expression that can be compared and cloned by its source text.
#[derive(Clone, Debug)]
pub struct Re(pub Regex);

impl PartialEq for Re {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}
impl Eq for Re {}

/// Where the photo number sits in a file name (the stem, without extension).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum NumberPattern {
    /// The last group of digits (`Project-0076` -> 76). The default.
    #[default]
    LastDigits,
    /// The first group of digits (`076_Anna` -> 76).
    FirstDigits,
    /// First capture group of a regular expression.
    Custom(Re),
}

impl NumberPattern {
    /// Parses the text a user types: empty or `last`, `first`, or a regular expression with a
    /// capture group (the first group is the number).
    pub fn parse(spec: &str) -> Result<NumberPattern, String> {
        let s = spec.trim();
        match s.to_ascii_lowercase().as_str() {
            "" | "last" => Ok(NumberPattern::LastDigits),
            "first" => Ok(NumberPattern::FirstDigits),
            _ => {
                let re = Regex::new(s).map_err(|e| format!("the number pattern is not a valid regular expression: {e}"))?;
                if re.captures_len() < 2 {
                    return Err("the number pattern needs a capture group, e.g. (\\d+)".into());
                }
                Ok(NumberPattern::Custom(Re(re)))
            }
        }
    }

    /// The text that parses back to this pattern.
    pub fn spec(&self) -> String {
        match self {
            NumberPattern::LastDigits => "last".into(),
            NumberPattern::FirstDigits => "first".into(),
            NumberPattern::Custom(r) => r.0.as_str().to_owned(),
        }
    }

    /// The photo number in `stem`, or `None` when the name has none (or it does not fit a u64).
    pub fn extract(&self, stem: &str) -> Option<u64> {
        match self {
            NumberPattern::LastDigits => digit_groups(stem).last().and_then(|g| g.parse().ok()),
            NumberPattern::FirstDigits => digit_groups(stem).next().and_then(|g| g.parse().ok()),
            NumberPattern::Custom(r) => {
                let caps = r.0.captures(stem)?;
                let m = caps.get(1)?.as_str().trim();
                if m.is_empty() || !m.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                m.parse().ok()
            }
        }
    }
}

fn digit_groups(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !c.is_ascii_digit()).filter(|g| !g.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_takes_the_last_digit_group() {
        let p = NumberPattern::default();
        assert_eq!(p.extract("Job2026-0076"), Some(76));
        assert_eq!(p.extract("IMG_5"), Some(5));
        assert_eq!(p.extract("a1b22c333"), Some(333));
        assert_eq!(p.extract("no digits"), None);
        assert_eq!(p.extract(""), None);
        assert_eq!(p.extract("x99999999999999999999999"), None, "overflow is not a number");
    }

    #[test]
    fn first_and_custom_patterns() {
        assert_eq!(NumberPattern::parse("first").unwrap().extract("076_Anna_2"), Some(76));
        let p = NumberPattern::parse(r"-(\d{4})_").unwrap();
        assert_eq!(p.extract("Job-0076_x-12"), Some(76));
        assert_eq!(p.extract("Job-76"), None);
        assert_eq!(NumberPattern::parse("LAST").unwrap(), NumberPattern::LastDigits);
        assert_eq!(NumberPattern::parse(&p.spec()).unwrap(), p, "the spec parses back");
    }

    #[test]
    fn bad_patterns_are_errors() {
        assert!(NumberPattern::parse("(unclosed").is_err());
        assert!(NumberPattern::parse(r"\d+").is_err(), "no capture group");
    }
}
