//! Reads real (generated) .xlsx files through calamine. The workbooks are built here with a minimal zip
//! writer (stored entries), so no archive crate is needed for the tests.

use std::path::Path;

use crate::plan::tests::{src, touch};
use crate::plan::{build, PlanOptions, Problem};
use crate::rules::RenameRules;
use crate::sheet::{CaptionSheet, SheetColumns, SheetError};

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn zip_stored(files: &[(String, String)]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    let u16le = |v: &mut Vec<u8>, x: u16| v.extend_from_slice(&x.to_le_bytes());
    let u32le = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
    for (name, content) in files {
        let data = content.as_bytes();
        let crc = crc32(data);
        let offset = out.len() as u32;
        u32le(&mut out, 0x0403_4b50);
        for x in [20u16, 0, 0, 0, 33] {
            u16le(&mut out, x);
        }
        u32le(&mut out, crc);
        u32le(&mut out, data.len() as u32);
        u32le(&mut out, data.len() as u32);
        u16le(&mut out, name.len() as u16);
        u16le(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        u32le(&mut central, 0x0201_4b50);
        for x in [20u16, 20, 0, 0, 0, 33] {
            u16le(&mut central, x);
        }
        u32le(&mut central, crc);
        u32le(&mut central, data.len() as u32);
        u32le(&mut central, data.len() as u32);
        for x in [name.len() as u16, 0, 0, 0, 0] {
            u16le(&mut central, x);
        }
        u32le(&mut central, 0);
        u32le(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    u32le(&mut out, 0x0605_4b50);
    for x in [0u16, 0, files.len() as u16, files.len() as u16] {
        u16le(&mut out, x);
    }
    u32le(&mut out, central.len() as u32);
    u32le(&mut out, cd_offset);
    u16le(&mut out, 0);
    out
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A cell of a generated sheet.
#[derive(Clone)]
enum Cell {
    Num(f64),
    Text(&'static str),
    /// A formula with its cached value.
    Formula(f64, &'static str),
    /// An Excel error cell (`#REF!`).
    Error(&'static str),
}

struct SheetDef {
    name: &'static str,
    /// `hidden` or `veryHidden`.
    state: Option<&'static str>,
    rows: Vec<Vec<Cell>>,
    /// Merged ranges ("C2:C4").
    merges: Vec<&'static str>,
}

fn sheet(name: &'static str, rows: Vec<Vec<Cell>>) -> SheetDef {
    SheetDef { name, state: None, rows, merges: Vec::new() }
}

/// Writes a workbook; `first_col` is the 0-based column of the first cell of every row.
fn write_workbook(path: &Path, first_col: usize, sheets: &[SheetDef]) {
    let mut strings: Vec<String> = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();
    let ns = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    let rel = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    let mut content_types = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/><Override PartName=\"/xl/sharedStrings.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml\"/>");
    let mut wb_sheets = String::new();
    let mut wb_rels = String::new();
    for (k, s) in sheets.iter().enumerate() {
        let n = k + 1;
        let mut rows = String::new();
        for (r, row) in s.rows.iter().enumerate() {
            rows.push_str(&format!("<row r=\"{}\">", r + 1));
            for (c, cell) in row.iter().enumerate() {
                let r_ref = format!("{}{}", crate::sheet::column_letters(first_col + c), r + 1);
                match cell {
                    Cell::Num(v) => rows.push_str(&format!("<c r=\"{r_ref}\"><v>{v}</v></c>")),
                    Cell::Formula(v, f) => rows.push_str(&format!("<c r=\"{r_ref}\"><f>{}</f><v>{v}</v></c>", esc(f))),
                    Cell::Error(e) => rows.push_str(&format!("<c r=\"{r_ref}\" t=\"e\"><v>{e}</v></c>")),
                    Cell::Text(t) => {
                        let i = strings.iter().position(|x| x == t).unwrap_or_else(|| {
                            strings.push((*t).to_owned());
                            strings.len() - 1
                        });
                        rows.push_str(&format!("<c r=\"{r_ref}\" t=\"s\"><v>{i}</v></c>"));
                    }
                }
            }
            rows.push_str("</row>");
        }
        let merges = if s.merges.is_empty() { String::new() } else { format!("<mergeCells count=\"{}\">{}</mergeCells>", s.merges.len(), s.merges.iter().map(|m| format!("<mergeCell ref=\"{m}\"/>")).collect::<String>()) };
        files.push((format!("xl/worksheets/sheet{n}.xml"), format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><worksheet xmlns=\"{ns}\"><sheetData>{rows}</sheetData>{merges}</worksheet>")));
        content_types.push_str(&format!("<Override PartName=\"/xl/worksheets/sheet{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>"));
        let state = s.state.map(|st| format!(" state=\"{st}\"")).unwrap_or_default();
        wb_sheets.push_str(&format!("<sheet name=\"{}\" sheetId=\"{n}\"{state} r:id=\"rId{n}\"/>", s.name));
        wb_rels.push_str(&format!("<Relationship Id=\"rId{n}\" Type=\"{rel}/worksheet\" Target=\"worksheets/sheet{n}.xml\"/>"));
    }
    content_types.push_str("</Types>");
    let sst: String = strings.iter().map(|s| format!("<si><t xml:space=\"preserve\">{}</t></si>", esc(s))).collect();
    let sst_id = sheets.len() + 1;
    files.push(("[Content_Types].xml".to_owned(), content_types));
    files.push(("_rels/.rels".to_owned(), format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{rel}/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>")));
    files.push(("xl/workbook.xml".to_owned(), format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><workbook xmlns=\"{ns}\" xmlns:r=\"{rel}\"><sheets>{wb_sheets}</sheets></workbook>")));
    files.push(("xl/_rels/workbook.xml.rels".to_owned(), format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{wb_rels}<Relationship Id=\"rId{sst_id}\" Type=\"{rel}/sharedStrings\" Target=\"sharedStrings.xml\"/></Relationships>")));
    files.push(("xl/sharedStrings.xml".to_owned(), format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><sst xmlns=\"{ns}\" count=\"{n}\" uniqueCount=\"{n}\">{sst}</sst>", n = strings.len())));
    std::fs::write(path, zip_stored(&files)).unwrap();
}

fn demo_rows() -> Vec<Vec<Cell>> {
    vec![
        vec![Cell::Text("Nr"), Cell::Text("Dateiname"), Cell::Text("Beschriftung")],
        vec![Cell::Num(75.0), Cell::Text("075_Anna Berger"), Cell::Text("Anna Berger, Tirol")],
        vec![Cell::Num(76.0), Cell::Text("076_Müller & Söhne"), Cell::Text("Team Müller & Söhne")],
        vec![Cell::Num(77.0), Cell::Text("077_Müller & Söhne"), Cell::Text("Team Müller & Söhne")],
    ]
}

fn load(p: &Path) -> Result<CaptionSheet, SheetError> {
    CaptionSheet::load(p, &SheetColumns::default(), None)
}

#[test]
fn reads_a_generated_xlsx_with_default_columns() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("captions.xlsx");
    write_workbook(&p, 0, &[sheet("Captions", demo_rows())]);
    let s = load(&p).unwrap();
    assert_eq!((s.header_rows, s.len()), (1, 3));
    assert_eq!(s.lookup(76).unwrap().name, "076_Müller & Söhne");
    assert_eq!(s.lookup(75).unwrap().caption, "Anna Berger, Tirol");
    assert_eq!(s.blocks().len(), 2, "Anna alone, then the Müller block of two");
    assert!(CaptionSheet::load(&p, &SheetColumns::default(), Some("captions")).is_ok());
    assert!(CaptionSheet::load(&p, &SheetColumns::default(), Some("nope")).is_err());
}

#[test]
fn a_sheet_that_starts_in_column_b_keeps_absolute_columns() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("shifted.xlsx");
    write_workbook(&p, 1, &[sheet("Captions", demo_rows())]);
    let s = CaptionSheet::load(&p, &SheetColumns { number: 1, name: 2, caption: 3 }, None).unwrap();
    assert_eq!(s.lookup(77).unwrap().name, "077_Müller & Söhne");
    assert_eq!(load(&p), Err(SheetError::NoRows));
}

#[test]
fn csv_and_garbage_files() {
    let t = tempfile::tempdir().unwrap();
    let c = t.path().join("c.csv");
    std::fs::write(&c, "Nr;Name;Caption\n76;076_Anna;Anna\n").unwrap();
    assert_eq!(load(&c).unwrap().lookup(76).unwrap().name, "076_Anna");
    let bad = t.path().join("bad.xlsx");
    std::fs::write(&bad, "not a zip").unwrap();
    assert!(matches!(load(&bad), Err(SheetError::Format(_))));
    assert!(matches!(load(&t.path().join("x.pdf")), Err(SheetError::Format(_))));
    assert!(matches!(load(&t.path().join("missing.csv")), Err(SheetError::Io(_))));
}

#[test]
fn hidden_sheets_are_skipped_for_the_default_but_can_be_named() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("hidden.xlsx");
    let old = vec![vec![Cell::Num(1.0), Cell::Text("001_OLD"), Cell::Text("old")]];
    let new = vec![vec![Cell::Num(1.0), Cell::Text("001_NEW"), Cell::Text("new")]];
    write_workbook(&p, 0, &[SheetDef { name: "Old", state: Some("hidden"), rows: old, merges: vec![] }, SheetDef { name: "Very", state: Some("veryHidden"), rows: vec![], merges: vec![] }, sheet("Current", new)]);
    assert_eq!(load(&p).unwrap().lookup(1).unwrap().name, "001_NEW", "the first visible sheet");
    assert_eq!(CaptionSheet::load(&p, &SheetColumns::default(), Some("Old")).unwrap().lookup(1).unwrap().name, "001_OLD", "a hidden sheet can still be chosen by name");
}

#[test]
fn formulas_text_numbers_and_floats() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("f.xlsx");
    let rows = vec![
        vec![Cell::Formula(76.0, "A3+1"), Cell::Formula(0.0, "\"076_\"&\"Anna\""), Cell::Text("Anna")],
        vec![Cell::Text("077"), Cell::Text("077_Bob"), Cell::Text("Bob")],
        vec![Cell::Num(78.0), Cell::Text("078_Cy"), Cell::Text("Cy")],
        vec![Cell::Num(1.5), Cell::Text("x"), Cell::Text("y")],
    ];
    write_workbook(&p, 0, &[sheet("S", rows)]);
    let s = load(&p).unwrap();
    assert_eq!(s.lookup(76).unwrap().caption, "Anna", "a formula gives its cached number");
    assert_eq!(s.lookup(77).unwrap().name, "077_Bob", "a number written as text counts");
    assert_eq!(s.lookup(78).unwrap().name, "078_Cy");
    assert_eq!(s.len(), 3, "1.5 is no photo number");
}

#[test]
fn excel_error_cells_block_the_sheet() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("e.xlsx");
    write_workbook(&p, 0, &[sheet("S", vec![vec![Cell::Num(1.0), Cell::Text("001_A"), Cell::Text("A")], vec![Cell::Num(2.0), Cell::Error("#REF!"), Cell::Text("A")]])]);
    let e = load(&p).unwrap_err();
    assert!(e.to_string().contains("#REF!") && e.to_string().contains("row 2"), "{e}");
}

#[test]
fn merged_caption_cells_leave_empty_captions_that_block_a_caption_rename() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("m.xlsx");
    let rows: Vec<Vec<Cell>> = vec![
        vec![Cell::Num(1.0), Cell::Text("001_Team"), Cell::Text("Team Rot")],
        vec![Cell::Num(2.0), Cell::Text("002_Team"), Cell::Text("")],
        vec![Cell::Num(3.0), Cell::Text("003_Team"), Cell::Text("")],
    ];
    write_workbook(&p, 0, &[SheetDef { name: "S", state: None, rows, merges: vec!["C1:C3"] }]);
    let sheet = load(&p).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let files: Vec<_> = (1..=3u64).map(|n| src(&touch(dir.path(), &format!("J-{n}.jpg"), "x"), n)).collect();
    // Names alone are fine; a template that needs the caption is blocked for the rows whose caption is empty.
    let ok = build(&files, &RenameRules { template: "{sheet_name}".into(), ..Default::default() }, Some(&sheet), &PlanOptions::default()).unwrap();
    assert!(!ok.blocked());
    let plan = build(&files, &RenameRules { template: "{no:03}_{caption}".into(), ..Default::default() }, Some(&sheet), &PlanOptions::default()).unwrap();
    assert!(plan.blocked());
    assert!(!plan.entries[0].problems.iter().any(|p| matches!(p, Problem::Sheet(_))));
    assert!(plan.entries[1].problems.iter().any(|p| matches!(p, Problem::Sheet(m) if m.contains("empty"))));
}
