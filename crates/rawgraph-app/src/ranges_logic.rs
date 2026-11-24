//! Pure logic of the range table of a Split by Metadata node: validation with
//! readable messages (the core only answers yes or no), adding and removing
//! rows, and the text of the CLI option `--split-ranges`. No egui types.

use rawgraph_core::graph::{MAX_RANGES, MAX_RANGE_NAME_CHARS};
use rawgraph_core::NamedRange;

/// Why a range table cannot be stored. The wording follows what the core checks.
#[derive(Clone, Debug, PartialEq)]
pub enum RangeProblem {
    TooMany,
    EmptyName(usize),
    NameTooLong(usize),
    DuplicateName(String),
    MinAboveMax(String),
    NotANumber(String),
}

impl std::fmt::Display for RangeProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RangeProblem::TooMany => write!(f, "At most {MAX_RANGES} ranges are possible"),
            RangeProblem::EmptyName(i) => write!(f, "Range {} has no name; every range needs a name", i + 1),
            RangeProblem::NameTooLong(i) => write!(f, "The name of range {} is longer than {MAX_RANGE_NAME_CHARS} characters", i + 1),
            RangeProblem::DuplicateName(n) => write!(f, "The name \"{n}\" is used twice; names must be unique"),
            RangeProblem::MinAboveMax(n) => write!(f, "\"{n}\": the minimum is above the maximum"),
            RangeProblem::NotANumber(n) => write!(f, "\"{n}\": minimum and maximum must be numbers"),
        }
    }
}

/// Checks a table the way `Node::set_data` does, and says what is wrong.
pub fn validate(rows: &[NamedRange]) -> Result<(), RangeProblem> {
    if rows.len() > MAX_RANGES {
        return Err(RangeProblem::TooMany);
    }
    for (i, r) in rows.iter().enumerate() {
        let n = r.name.chars().count();
        if n == 0 {
            return Err(RangeProblem::EmptyName(i));
        }
        if n > MAX_RANGE_NAME_CHARS {
            return Err(RangeProblem::NameTooLong(i));
        }
        if !r.min.is_finite() || !r.max.is_finite() {
            return Err(RangeProblem::NotANumber(r.name.clone()));
        }
        if r.min > r.max {
            return Err(RangeProblem::MinAboveMax(r.name.clone()));
        }
        if rows[..i].iter().any(|o| o.name == r.name) {
            return Err(RangeProblem::DuplicateName(r.name.clone()));
        }
    }
    Ok(())
}

/// A new row after the last one: a free name and a range that starts above the last maximum.
pub fn next_row(rows: &[NamedRange]) -> Option<NamedRange> {
    if rows.len() >= MAX_RANGES {
        return None;
    }
    let mut n = rows.len() + 1;
    let name = loop {
        let candidate = format!("Range {n}");
        if rows.iter().all(|r| r.name != candidate) {
            break candidate;
        }
        n += 1;
    };
    let min = rows.last().map_or(0.0, |r| (r.max + 1.0).floor());
    Some(NamedRange { name, min, max: min + 99.0 })
}

/// Removes row `idx`; returns whether it existed.
pub fn remove_row(rows: &mut Vec<NamedRange>, idx: usize) -> bool {
    if idx < rows.len() {
        rows.remove(idx);
        true
    } else {
        false
    }
}

/// `--split-ranges` value after the node index: `name=min..max[,name=min..max...]`.
pub fn parse_ranges(text: &str) -> Result<Vec<NamedRange>, String> {
    let mut rows = Vec::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let bad = || format!("\"{part}\" is not of the form <name>=<min>..<max>");
        let (name, range) = part.split_once('=').ok_or_else(bad)?;
        let (lo, hi) = range.split_once("..").ok_or_else(bad)?;
        let lo = lo.trim().parse::<f32>().map_err(|_| bad())?;
        let hi = hi.trim().parse::<f32>().map_err(|_| bad())?;
        rows.push(NamedRange { name: name.trim().to_owned(), min: lo, max: hi });
    }
    validate(&rows).map_err(|e| e.to_string())?;
    Ok(rows)
}

/// "400" instead of "400.0", "0.5" stays.
pub fn number_text(v: f32) -> String {
    if (v - v.round()).abs() < 1e-4 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_core::{Graph, ImageId, NodeData, NodeKind};

    fn r(name: &str, min: f32, max: f32) -> NamedRange {
        NamedRange { name: name.to_owned(), min, max }
    }

    #[test]
    fn every_problem_has_its_message() {
        assert_eq!(validate(&[r("Low", 0.0, 400.0), r("High", 401.0, 6400.0)]), Ok(()));
        assert_eq!(validate(&[]), Ok(()), "no ranges at all is fine: everything is unmatched");
        let e = validate(&[r("", 0.0, 1.0)]).unwrap_err();
        assert_eq!(e, RangeProblem::EmptyName(0));
        assert!(e.to_string().contains("no name"), "{e}");
        let e = validate(&[r("A", 5.0, 1.0)]).unwrap_err();
        assert!(matches!(e, RangeProblem::MinAboveMax(_)) && e.to_string().contains("minimum is above the maximum"), "{e}");
        let e = validate(&[r("A", 0.0, 1.0), r("A", 2.0, 3.0)]).unwrap_err();
        assert_eq!(e, RangeProblem::DuplicateName("A".to_owned()));
        assert!(e.to_string().contains("used twice"));
        let many: Vec<NamedRange> = (0..7).map(|i| r(&format!("R{i}"), 0.0, 1.0)).collect();
        let e = validate(&many).unwrap_err();
        assert_eq!(e, RangeProblem::TooMany);
        assert!(e.to_string().contains("At most 6"));
        let long = "x".repeat(41);
        assert!(validate(&[r(&long, 0.0, 1.0)]).unwrap_err().to_string().contains("longer than 40"));
        assert!(validate(&[r("A", f32::NAN, 1.0)]).unwrap_err().to_string().contains("numbers"));
        assert!(validate(&[r(&"x".repeat(40), 0.0, 1.0)]).is_ok());
    }

    #[test]
    fn the_core_agrees_with_every_message() {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::MetadataSplit, [0.0, 0.0]);
        let many: Vec<NamedRange> = (0..7).map(|i| r(&format!("R{i}"), 0.0, 1.0)).collect();
        let cases = [
            vec![r("", 0.0, 1.0)],
            vec![r("A", 5.0, 1.0)],
            vec![r("A", 0.0, 1.0), r("A", 2.0, 3.0)],
            many,
            vec![r(&"x".repeat(41), 0.0, 1.0)],
            vec![r("A", f32::NAN, 1.0)],
            vec![r("Low", 0.0, 400.0), r("High", 401.0, 6400.0)],
            vec![],
        ];
        for rows in cases {
            let mine = validate(&rows).is_ok();
            let core = g.node_mut(id).unwrap().set_data(ImageId(1), Some(NodeData::Ranges(rows.clone())));
            assert_eq!(mine, core, "validator and core disagree on {rows:?}");
        }
    }

    #[test]
    fn rows_are_added_up_to_six_and_removed() {
        let mut rows: Vec<NamedRange> = Vec::new();
        for _ in 0..MAX_RANGES {
            let row = next_row(&rows).expect("room for another row");
            rows.push(row);
            assert_eq!(validate(&rows), Ok(()), "every added row keeps the table valid: {rows:?}");
        }
        assert_eq!(rows.len(), 6);
        assert!(next_row(&rows).is_none(), "the seventh is refused");
        assert!(remove_row(&mut rows, 2));
        assert_eq!(rows.len(), 5);
        assert!(!remove_row(&mut rows, 9));
        let again = next_row(&rows).unwrap();
        assert!(rows.iter().all(|x| x.name != again.name), "the new name is free: {}", again.name);
        assert_eq!(next_row(&[]).unwrap(), r("Range 1", 0.0, 99.0));
        let after = next_row(&[r("Low", 0.0, 400.0)]).unwrap();
        assert_eq!((after.min, after.max), (401.0, 500.0));
    }

    #[test]
    fn the_cli_text_is_parsed_and_validated() {
        let rows = parse_ranges("Low=0..400,High=401..6400").unwrap();
        assert_eq!(rows, vec![r("Low", 0.0, 400.0), r("High", 401.0, 6400.0)]);
        assert_eq!(parse_ranges("Wide light = 0.5 .. 2.5").unwrap(), vec![r("Wide light", 0.5, 2.5)]);
        assert_eq!(parse_ranges("").unwrap(), vec![]);
        for bad in ["Low", "Low=0", "Low=a..b", "Low=5..1", "A=0..1,A=2..3", "=0..1"] {
            assert!(parse_ranges(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn numbers_read_well() {
        assert_eq!(number_text(400.0), "400");
        assert_eq!(number_text(0.5), "0.5");
    }
}
