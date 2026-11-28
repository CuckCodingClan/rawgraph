//! Caption sheets: a spreadsheet (.xlsx or .csv) that maps a photo number to the final file name
//! and a caption. Column A = number, column B = target name (`076_Anna Berger`), column C = caption
//! by default; the columns can be changed. Rows above the first numeric row (a header) and rows
//! without a number are ignored.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::numbers::NumberPattern;

/// Zero-based column numbers. Defaults: A, B, C.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SheetColumns {
    pub number: usize,
    pub name: usize,
    pub caption: usize,
}

impl Default for SheetColumns {
    fn default() -> Self {
        SheetColumns { number: 0, name: 1, caption: 2 }
    }
}

/// "A" -> 0, "B" -> 1, "AA" -> 26. Case-insensitive; `None` for anything else.
pub fn column_from_letters(s: &str) -> Option<usize> {
    let s = s.trim();
    if s.is_empty() || s.len() > 3 || !s.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut n = 0usize;
    for c in s.chars() {
        n = n * 26 + (c.to_ascii_uppercase() as usize - 'A' as usize + 1);
    }
    Some(n - 1)
}

/// 0 -> "A", 26 -> "AA".
pub fn column_letters(mut n: usize) -> String {
    let mut out = Vec::new();
    loop {
        out.push((b'A' + (n % 26) as u8) as char);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    out.iter().rev().collect()
}

/// One usable row of the sheet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SheetRow {
    pub number: u64,
    /// Column B: the target file name without extension.
    pub name: String,
    /// Column C.
    pub caption: String,
    /// 1-based row in the file.
    pub row: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SheetError {
    Io(String),
    Format(String),
    NoRows,
}

impl std::fmt::Display for SheetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SheetError::Io(m) | SheetError::Format(m) => f.write_str(m),
            SheetError::NoRows => f.write_str("the sheet has no row with a photo number in the number column"),
        }
    }
}

impl std::error::Error for SheetError {}

/// Why a number has no unique row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LookupError {
    NotFound,
    /// The sheet has this number several times with different content (the rows).
    Ambiguous(Vec<usize>),
}

/// The parsed sheet. Immutable after loading.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptionSheet {
    rows: Vec<SheetRow>,
    by_number: BTreeMap<u64, Vec<usize>>,
    /// Rows above the first numeric row.
    pub header_rows: usize,
    /// Rows after the header that have no usable number (notes, empty lines).
    pub ignored_rows: usize,
    pub source: Option<PathBuf>,
}

impl CaptionSheet {
    /// Builds the sheet from raw cell text (one `Vec` per row, absolute columns).
    pub fn from_cells(cells: &[Vec<String>], cols: &SheetColumns) -> Result<CaptionSheet, SheetError> {
        let mut sheet = CaptionSheet::default();
        let mut seen_number = false;
        for (i, row) in cells.iter().enumerate() {
            let cell = |c: usize| row.get(c).map(|s| s.trim()).unwrap_or("");
            match parse_number(cell(cols.number)) {
                None => {
                    if seen_number {
                        if row.iter().any(|c| !c.trim().is_empty()) {
                            sheet.ignored_rows += 1;
                        }
                    } else {
                        sheet.header_rows += 1;
                    }
                }
                Some(number) => {
                    seen_number = true;
                    let r = SheetRow { number, name: cell(cols.name).to_owned(), caption: cell(cols.caption).to_owned(), row: i + 1 };
                    let list = sheet.by_number.entry(number).or_default();
                    // The same row twice is no ambiguity.
                    if list.iter().any(|&k| sheet.rows[k].name == r.name && sheet.rows[k].caption == r.caption) {
                        continue;
                    }
                    list.push(sheet.rows.len());
                    sheet.rows.push(r);
                }
            }
        }
        if sheet.rows.is_empty() {
            return Err(SheetError::NoRows);
        }
        Ok(sheet)
    }

    /// Loads `.xlsx` / `.xlsm` / `.xls` / `.ods` (first sheet, or `sheet_name`) or `.csv` / `.tsv` / `.txt`.
    pub fn load(path: &Path, cols: &SheetColumns, sheet_name: Option<&str>) -> Result<CaptionSheet, SheetError> {
        let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let cells = match ext.as_str() {
            "csv" | "tsv" | "txt" => {
                let bytes = std::fs::read(path).map_err(|e| SheetError::Io(format!("{}: {e}", path.display())))?;
                parse_csv(&decode_text(&bytes))
            }
            "xlsx" | "xlsm" | "xls" | "xlsb" | "ods" => read_workbook(path, sheet_name)?,
            _ => return Err(SheetError::Format(format!("{} is not a spreadsheet (.xlsx or .csv expected)", path.display()))),
        };
        let mut sheet = Self::from_cells(&cells, cols)?;
        sheet.source = Some(path.to_path_buf());
        Ok(sheet)
    }

    pub fn rows(&self) -> &[SheetRow] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The row of a photo number.
    pub fn lookup(&self, number: u64) -> Result<&SheetRow, LookupError> {
        match self.by_number.get(&number).map(Vec::as_slice) {
            None | Some([]) => Err(LookupError::NotFound),
            Some([one]) => Ok(&self.rows[*one]),
            Some(many) => Err(LookupError::Ambiguous(many.iter().map(|&k| self.rows[k].row).collect())),
        }
    }

    /// All numbers of the sheet, ascending.
    pub fn numbers(&self) -> Vec<u64> {
        self.by_number.keys().copied().collect()
    }

    /// Numbers that appear in several rows with different content.
    pub fn ambiguous_numbers(&self) -> Vec<u64> {
        self.by_number.iter().filter(|(_, v)| v.len() > 1).map(|(n, _)| *n).collect()
    }

    /// Blocks of consecutive numbers (n, n+1, ...) that share one caption, ascending.
    pub fn blocks(&self) -> Vec<SheetBlock> {
        let mut out: Vec<SheetBlock> = Vec::new();
        for (&n, list) in &self.by_number {
            let Some(&k) = list.first() else { continue };
            let caption = &self.rows[k].caption;
            match out.last_mut() {
                Some(b) if b.last_no + 1 == n && &b.caption == caption => b.last_no = n,
                _ => out.push(SheetBlock { caption: caption.clone(), first_no: n, last_no: n }),
            }
        }
        out
    }
}

/// Consecutive photo numbers with the same caption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SheetBlock {
    pub caption: String,
    pub first_no: u64,
    pub last_no: u64,
}

/// A sheet together with the rule that finds the number in a file name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SheetBinding {
    pub sheet: CaptionSheet,
    pub pattern: NumberPattern,
}

/// What a file name resolves to in a sheet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub number: u64,
    pub name: String,
    pub caption: String,
}

impl SheetBinding {
    /// Photo number of `stem` and its sheet row. The errors are plain sentences for the user.
    pub fn resolve(&self, stem: &str) -> Result<Resolved, String> {
        let number = self.pattern.extract(stem).ok_or_else(|| format!("no photo number found in the file name {stem:?}"))?;
        match self.sheet.lookup(number) {
            Ok(r) => Ok(Resolved { number, name: r.name.clone(), caption: r.caption.clone() }),
            Err(LookupError::NotFound) => Err(format!("photo {number} has no row in the caption sheet")),
            Err(LookupError::Ambiguous(rows)) => Err(format!("photo {number} has several different rows in the caption sheet (rows {})", join_rows(&rows))),
        }
    }
}

fn join_rows(rows: &[usize]) -> String {
    rows.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(", ")
}

/// A cell is a photo number when it is only digits, or a whole number written as `76.0`.
fn parse_number(cell: &str) -> Option<u64> {
    let c = cell.trim();
    if c.is_empty() {
        return None;
    }
    let digits = match c.split_once('.') {
        Some((i, f)) if !f.is_empty() && f.chars().all(|d| d == '0') => i,
        _ => c,
    };
    if digits.is_empty() || !digits.chars().all(|d| d.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

// ------------------------------------------------------------------ csv

/// UTF-8 (with or without BOM); anything else is read as Latin-1 / Windows-1252 for the letters
/// that matter here (umlauts, ß).
fn decode_text(bytes: &[u8]) -> String {
    let b = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(b) {
        Ok(s) => s.to_owned(),
        Err(_) => b.iter().map(|&x| char::from(x)).collect(),
    }
}

/// Delimiter with the most occurrences outside quotes in the first non-empty line (`;` before `,`
/// before tab on ties).
fn detect_delimiter(text: &str) -> char {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut counts: HashMap<char, usize> = HashMap::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            ';' | ',' | '\t' if !quoted => *counts.entry(c).or_default() += 1,
            _ => {}
        }
    }
    let mut best = (0usize, ';');
    for d in [';', ',', '\t'] {
        let n = counts.get(&d).copied().unwrap_or(0);
        if n > best.0 {
            best = (n, d);
        }
    }
    best.1
}

/// RFC-4180-style parsing with quotes, doubled quotes and line breaks inside quoted cells.
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let delim = detect_delimiter(text);
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cell.push('"');
                } else {
                    quoted = false;
                }
            } else {
                cell.push(c);
            }
            continue;
        }
        match c {
            '"' if cell.is_empty() => quoted = true,
            c if c == delim => row.push(std::mem::take(&mut cell)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
            }
            c => cell.push(c),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

// ----------------------------------------------------------------- xlsx

fn cell_text(d: &calamine::Data) -> String {
    use calamine::Data;
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => {
            if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            }
        }
        Data::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn read_workbook(path: &Path, sheet_name: Option<&str>) -> Result<Vec<Vec<String>>, SheetError> {
    use calamine::{open_workbook_auto, Reader};
    let mut wb = open_workbook_auto(path).map_err(|e| SheetError::Format(format!("{}: {e}", path.display())))?;
    let names = wb.sheet_names();
    let name = match sheet_name.filter(|s| !s.is_empty()) {
        Some(n) => names.iter().find(|x| x.eq_ignore_ascii_case(n)).cloned().ok_or_else(|| SheetError::Format(format!("the workbook has no sheet named {n:?}")))?,
        None => names.first().cloned().ok_or_else(|| SheetError::Format("the workbook has no sheets".into()))?,
    };
    let range = wb.worksheet_range(&name).map_err(|e| SheetError::Format(format!("sheet {name:?}: {e}")))?;
    let (row0, col0) = range.start().map(|(r, c)| (r as usize, c as usize)).unwrap_or((0, 0));
    let mut rows: Vec<Vec<String>> = vec![Vec::new(); row0];
    for r in range.rows() {
        let mut v = vec![String::new(); col0];
        v.extend(r.iter().map(cell_text));
        rows.push(v);
    }
    Ok(rows)
}

/// One line about a loaded sheet, for display.
pub fn describe(sheet: &CaptionSheet) -> String {
    let name = sheet.source.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "sheet".into());
    format!("{name}: {} rows ({} header, {} ignored)", sheet.len(), sheet.header_rows, sheet.ignored_rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter().map(|r| r.iter().map(|s| (*s).to_owned()).collect()).collect()
    }

    #[test]
    fn columns_by_letters() {
        assert_eq!(column_from_letters("A"), Some(0));
        assert_eq!(column_from_letters(" c "), Some(2));
        assert_eq!(column_from_letters("AA"), Some(26));
        assert_eq!(column_from_letters(""), None);
        assert_eq!(column_from_letters("1"), None);
        assert_eq!(column_letters(0), "A");
        assert_eq!(column_letters(25), "Z");
        assert_eq!(column_letters(26), "AA");
        assert_eq!(column_from_letters(&column_letters(700)), Some(700));
    }

    #[test]
    fn header_and_notes_are_ignored_and_rows_looked_up() {
        let c = cells(&[&["Nr", "Name", "Caption"], &["76", "076_Anna", "Anna Berger"], &["77.0", "077_Anna", "Anna Berger"], &["", "", ""], &["Note", "x", "y"], &["78", "078_Bob", "Bob"]]);
        let s = CaptionSheet::from_cells(&c, &SheetColumns::default()).unwrap();
        assert_eq!((s.header_rows, s.ignored_rows, s.len()), (1, 1, 3));
        assert_eq!(s.lookup(77).unwrap().name, "077_Anna");
        assert_eq!(s.lookup(78).unwrap().row, 6);
        assert_eq!(s.lookup(5), Err(LookupError::NotFound));
        assert_eq!(s.numbers(), vec![76, 77, 78]);
    }

    #[test]
    fn identical_duplicates_are_merged_different_ones_are_ambiguous() {
        let c = cells(&[&["1", "001_A", "A"], &["1", "001_A", "A"], &["2", "002_A", "A"], &["2", "002_B", "B"]]);
        let s = CaptionSheet::from_cells(&c, &SheetColumns::default()).unwrap();
        assert!(s.lookup(1).is_ok());
        assert_eq!(s.lookup(2), Err(LookupError::Ambiguous(vec![3, 4])));
        assert_eq!(s.ambiguous_numbers(), vec![2]);
    }

    #[test]
    fn other_columns_and_no_rows() {
        let c = cells(&[&["x", "7", "007_Z", "cap"]]);
        let s = CaptionSheet::from_cells(&c, &SheetColumns { number: 1, name: 2, caption: 3 }).unwrap();
        assert_eq!(s.lookup(7).unwrap().caption, "cap");
        assert_eq!(CaptionSheet::from_cells(&cells(&[&["a", "b"]]), &SheetColumns::default()), Err(SheetError::NoRows));
    }

    #[test]
    fn blocks_are_consecutive_numbers_with_one_caption() {
        let c = cells(&[&["1", "", "A"], &["2", "", "A"], &["3", "", "B"], &["5", "", "B"], &["6", "", "B"], &["7", "", "A"]]);
        let b = CaptionSheet::from_cells(&c, &SheetColumns::default()).unwrap().blocks();
        let flat: Vec<(&str, u64, u64)> = b.iter().map(|x| (x.caption.as_str(), x.first_no, x.last_no)).collect();
        assert_eq!(flat, vec![("A", 1, 2), ("B", 3, 3), ("B", 5, 6), ("A", 7, 7)], "a gap in the numbers starts a new block");
    }

    #[test]
    fn csv_with_semicolons_quotes_bom_and_latin1() {
        let text = "\u{feff}Nr;Name;Caption\r\n1;001_M\u{fc}ller;\"Anna \"\"A\"\"; Berger\"\r\n2;002_x;multi\n";
        let rows = parse_csv(text);
        assert_eq!(rows[1], vec!["1", "001_Müller", "Anna \"A\"; Berger"]);
        let s = CaptionSheet::from_cells(&decode_rows(text), &SheetColumns::default()).unwrap();
        assert_eq!(s.lookup(2).unwrap().caption, "multi");
        // Latin-1 bytes
        assert_eq!(decode_text(&[b'M', 0xFC, b'l']), "Mül");
        // Comma and tab delimiters are detected.
        assert_eq!(parse_csv("1,a,b\n")[0], vec!["1", "a", "b"]);
        assert_eq!(parse_csv("1\ta;x\tb\n")[0], vec!["1", "a;x", "b"]);
    }

    fn decode_rows(t: &str) -> Vec<Vec<String>> {
        parse_csv(t.strip_prefix('\u{feff}').unwrap_or(t))
    }

    #[test]
    fn resolve_gives_plain_errors() {
        let c = cells(&[&["76", "076_Anna", "Anna"], &["9", "009_a", "a"], &["9", "009_b", "b"]]);
        let b = SheetBinding { sheet: CaptionSheet::from_cells(&c, &SheetColumns::default()).unwrap(), pattern: NumberPattern::default() };
        assert_eq!(b.resolve("Job-0076").unwrap().name, "076_Anna");
        assert!(b.resolve("Job-0077").unwrap_err().contains("no row"));
        assert!(b.resolve("Job").unwrap_err().contains("no photo number"));
        assert!(b.resolve("x9").unwrap_err().contains("several"));
    }
}
